//! A turn's transaction: participants set their records in one WAL, the
//! host flushes it in one put, and tells them; a participant that fails to
//! prepare abandons the turn, which runs again from the last seal.
mod common;

use anyhow::{Result, bail};
use async_trait::async_trait;
use common::{Store, attempt, programs, settle};
use functions::{
    Attached, Extension, Extensions, Outcome, Participant, Record, Run, ScopeBlobs, State, Wal,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// An extension that counts the seals it takes part in, in its record
/// (`counter/state`), and logs what the host tells it.
#[derive(Clone, Default)]
struct Counter {
    log: Arc<Mutex<Vec<String>>>,
    /// Whether its next prepare fails.
    fail: Arc<AtomicBool>,
}

impl Counter {
    fn log(&self) -> Vec<String> {
        std::mem::take(&mut self.log.lock().unwrap())
    }
}

/// The extension's part of one attempt at a run.
struct Counting {
    counter: Counter,
    record: Record,
    /// As this turn leaves it.
    count: u64,
    /// As the last seal holds it, which an aborted turn returns to.
    sealed: u64,
}

#[async_trait]
impl Extension for Counter {
    /// Every attempt starts here: from the count the scope's last seal holds.
    async fn attach(&self, _blobs: ScopeBlobs, state: State) -> Result<Box<dyn Attached>> {
        let record = state.record("state");
        let saved: Value = serde_json::from_slice(&record.get()).unwrap_or(Value::Null);
        let count = saved["count"].as_u64().unwrap_or(0);
        Ok(Box::new(Counting {
            counter: self.clone(),
            record,
            count,
            sealed: count,
        }))
    }
}

#[async_trait]
impl Participant for Counting {
    /// Before the seal: one more, set in the WAL. Its flush makes the count
    /// durable with the run's memory, its tables and every other
    /// participant, in one put.
    async fn prepare(&mut self, wal: &mut Wal) -> Result<()> {
        if self.counter.fail.swap(false, Ordering::SeqCst) {
            bail!("Injected prepare failure");
        }
        self.count += 1;
        let bytes = serde_json::to_vec(&json!({"count": self.count}))?;
        (wal.set(&self.record, bytes.into())).map_err(anyhow::Error::from_boxed)?;
        Ok(())
    }

    fn committed(&mut self) {
        self.sealed = self.count;
        let mut log = self.counter.log.lock().unwrap();
        log.push(format!("committed {}", self.count));
    }

    fn aborted(&mut self) {
        self.count = self.sealed;
        self.counter.log.lock().unwrap().push("aborted".into());
    }
}

impl Attached for Counting {}

const TWO_SLEEPS: &str = r#"
export async function main(_, ctx) {
  await ctx.sleep(5);
  await ctx.sleep(5);
  return 'done';
}
"#;

fn offered(counter: &Counter) -> Extensions {
    Extensions::new().with("counter", counter.clone())
}

#[tokio::test]
async fn each_seal_holds_every_record_and_the_next_starts_from_it() {
    let counter = Counter::default();
    let store = Store::default();
    let catalog = programs(TWO_SLEEPS);
    let mut run = attempt(&store, &catalog, Value::Null, &offered(&counter)).await;
    assert_eq!(settle(&mut run).await, json!("done"));
    assert_eq!(counter.log(), ["committed 1", "committed 2", "committed 3"]);
    assert_eq!(store.seals(), 3);
    for name in ["memory", "run"] {
        assert!(store.record(name).is_object(), "{name}");
    }
    assert_eq!(store.record("counter/state"), json!({"count": 3}));

    // The next run in the scope: the extension's record carries over, as the
    // scope's tables do.
    let scope = store.scope().extended(offered(&counter));
    let mut next = Run::start(&catalog, &scope, "s", "r2", "v1", "main", Value::Null, 20)
        .await
        .unwrap();
    settle(&mut next).await;
    assert_eq!(store.seals(), 6);
    assert_eq!(store.record("counter/state"), json!({"count": 6}));
}

#[tokio::test]
async fn a_participant_that_fails_to_prepare_abandons_the_turn() {
    let counter = Counter::default();
    let store = Store::default();
    let catalog = programs(TWO_SLEEPS);
    let mut run = attempt(&store, &catalog, Value::Null, &offered(&counter)).await;
    counter.log();
    let sealed = store.values();
    let timer = run.view().state.pending[0].clone();

    // The second turn's prepare fails: nothing is sealed, every participant
    // hears it, and the attempt is over.
    counter.fail.store(true, Ordering::SeqCst);
    let delivered = run
        .deliver(timer.id(), Outcome::success(Value::Null), 10)
        .await;
    assert!(format!("{:#}", delivered.unwrap_err()).contains("Injected prepare failure"));
    assert_eq!(counter.log(), ["aborted"]);
    assert_eq!(store.values(), sealed);
    assert!(
        run.deliver(timer.id(), Outcome::success(Value::Null), 10)
            .await
            .is_err()
    );

    // Another attempt starts from the last seal, and the turn runs again.
    let mut run = attempt(&store, &catalog, Value::Null, &offered(&counter)).await;
    assert_eq!(settle(&mut run).await, json!("done"));
    assert_eq!(counter.log(), ["committed 2", "committed 3"]);
}
