//! Signals to programs: `ctx.signal(name, input)` waits for a value from
//! outside (a person's approval, another system's result), and the object's
//! `signal` handler gives one. The waits are kept by `function_signals`, a
//! journaled object keyed `<program>:<key>`, whose `wait` returns once
//! `resolve` gives the wait its value.
//!
//! A program's handler cannot wait on an awakeable itself (it has no journal
//! to replay one from), so it calls `wait`, under the request's idempotency
//! key: a retried attempt joins the same wait, and the run parks its memory
//! while it waits, as for any call.
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

/// The key of the waits of `program`'s object `key`.
pub fn signals_of(program: &str, key: &str) -> String {
    format!("{program}:{key}")
}

/// A wait for a signal, as `pending` lists it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Waiting {
    /// The wait's id (the run's request, `<run>/<n>`): `signal` takes it to resolve this wait and no other.
    pub id: String,
    /// The signal it waits for, such as `approval`.
    pub name: String,
    /// What it waits for, as the program said it (such as the refund to approve).
    #[serde(default)]
    pub input: Value,
    /// When it started, in milliseconds since the epoch.
    #[serde(default)]
    pub since: u64,
}

/// Which wait a signal resolves, and with what: the wait `id`, or else the
/// oldest for the signal `name`. With `keep`, a signal for a name that no
/// one waits for is kept for the next wait for it; else it is dropped.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Signal {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub keep: bool,
}

/// What `signal` did: the id of the wait it resolved, or None if none waited.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Resolved {
    pub resolved: Option<String>,
}

/// A wait, and the awakeable that resolves it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Listening {
    #[serde(flatten)]
    waiting: Waiting,
    awakeable: String,
}

/// The object's state: the waits, oldest first; the signals kept for the
/// next wait, by name.
const WAITING: &str = "waiting";
const KEPT: &str = "kept";
type Kept = BTreeMap<String, Vec<Value>>;

/// The `function_signals` object: the waits of every program's objects, each
/// under its key ([`signals_of`]).
pub struct FunctionSignals;

#[restate_sdk::object(name = "function_signals")]
impl FunctionSignals {
    /// Wait for the signal `waiting.name`, as wait `waiting.id`, and return its value.
    #[handler]
    async fn wait(
        &self,
        ctx: SharedObjectContext<'_>,
        waiting: Json<Waiting>,
    ) -> HandlerResult<Json<Value>> {
        let mut waiting = waiting.into_inner();
        let (awakeable, value) = ctx.awakeable::<Json<Value>>();
        waiting.since = ctx.run(|| async { Ok(now()) }).name("since").await?;
        let listening = Listening { waiting, awakeable };
        ctx.object_client::<FunctionSignalsClient>(ctx.key())
            .listen(Json(listening))
            .send();
        Ok(value.await?)
    }

    /// Resolve the wait `signal.id`, or else the oldest wait for `signal.name`, with `signal.value`.
    #[handler]
    async fn resolve(
        &self,
        ctx: ObjectContext<'_>,
        signal: Json<Signal>,
    ) -> HandlerResult<Json<Resolved>> {
        let signal = signal.into_inner();
        let mut waiting = waits(&ctx).await?;
        let at = match (&signal.id, &signal.name) {
            (Some(id), _) => waiting.iter().position(|w| &w.waiting.id == id),
            (None, Some(name)) => waiting.iter().position(|w| &w.waiting.name == name),
            (None, None) => return Err(TerminalError::new("A signal needs an id or a name").into()),
        };
        let Some(at) = at else {
            if let (true, None, Some(name)) = (signal.keep, &signal.id, &signal.name) {
                let mut kept = ctx
                    .get::<Json<Kept>>(KEPT)
                    .await?
                    .map(Json::into_inner)
                    .unwrap_or_default();
                kept.entry(name.clone()).or_default().push(signal.value);
                ctx.set(KEPT, Json(kept));
            }
            return Ok(Json(Resolved { resolved: None }));
        };
        let found = waiting.remove(at);
        ctx.resolve_awakeable(&found.awakeable, Json(signal.value));
        ctx.set(WAITING, Json(waiting));
        Ok(Json(Resolved {
            resolved: Some(found.waiting.id),
        }))
    }

    /// The waits, oldest first.
    #[handler]
    async fn pending(&self, ctx: SharedObjectContext<'_>) -> HandlerResult<Json<Vec<Waiting>>> {
        let waiting = (ctx.get::<Json<Vec<Listening>>>(WAITING).await?)
            .map(Json::into_inner)
            .unwrap_or_default();
        Ok(Json(waiting.into_iter().map(|w| w.waiting).collect()))
    }

    /// A wait: resolved now with a signal kept for it, or else when one comes.
    #[handler]
    async fn listen(
        &self,
        ctx: ObjectContext<'_>,
        listening: Json<Listening>,
    ) -> HandlerResult<()> {
        let listening = listening.into_inner();
        let mut kept = ctx
            .get::<Json<Kept>>(KEPT)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        if let Some(values) = kept.get_mut(&listening.waiting.name)
            && !values.is_empty()
        {
            let value = values.remove(0);
            ctx.resolve_awakeable(&listening.awakeable, Json(value));
            ctx.set(KEPT, Json(kept));
            return Ok(());
        }
        let mut waiting = waits(&ctx).await?;
        waiting.push(listening);
        ctx.set(WAITING, Json(waiting));
        Ok(())
    }

    /// Forget the waits `ids`, whose run no longer waits (it settled first):
    /// each ends, so that its `wait` completes.
    #[handler]
    async fn forget(&self, ctx: ObjectContext<'_>, ids: Json<Vec<String>>) -> HandlerResult<()> {
        let ids = ids.into_inner();
        let (gone, waiting): (Vec<_>, Vec<_>) = (waits(&ctx).await?)
            .into_iter()
            .partition(|w| ids.contains(&w.waiting.id));
        for wait in gone {
            ctx.reject_awakeable(
                &wait.awakeable,
                TerminalError::new("No one waits for this signal any more"),
            );
        }
        ctx.set(WAITING, Json(waiting));
        Ok(())
    }
}

async fn waits(ctx: &ObjectContext<'_>) -> Result<Vec<Listening>, TerminalError> {
    Ok((ctx.get::<Json<Vec<Listening>>>(WAITING).await?)
        .map(Json::into_inner)
        .unwrap_or_default())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
