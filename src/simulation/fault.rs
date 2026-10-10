//! Deterministic simulator failpoints. Not a production backend or remote fencing.
use super::{MemoryKv, yield_once};
use crate::kv::KvStore;
use futures::{channel::oneshot, future::BoxFuture};
use std::{io, sync::Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Get,
    Put,
    Delete,
}

#[derive(Clone, Copy, Debug)]
pub enum Effect {
    /// Fail before touching the underlying store.
    None,
    /// Atomic visible-only mutation; reads behave like After.
    Visible,
    /// Complete the underlying operation, then lose its acknowledgement.
    After,
}

#[derive(Clone, Debug)]
pub struct Failpoint {
    pub operation: Operation,
    pub key_prefix: String,
    /// One-based ordinal among matching operations since arm().
    pub nth: usize,
    pub effect: Effect,
    /// Signal the caller and stay pending until it drops the future.
    pub cancel: bool,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub operation: Operation,
    pub key: String,
    pub faulted: bool,
}

#[derive(Default)]
struct Control {
    plan: Option<Failpoint>,
    seen: usize,
    signal: Option<oneshot::Sender<()>>,
    events: Vec<Event>,
    active: usize,
    max_active: usize,
}

/// Each delayed request remains a local future: dropping it settles it. Real
/// transports may outlive their futures and still require external host fencing.
pub struct FaultKv {
    pub inner: MemoryKv,
    seed: u64,
    control: Mutex<Control>,
}

struct Active<'a>(&'a FaultKv);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.control.lock().unwrap().active -= 1;
    }
}

impl FaultKv {
    pub fn new(inner: MemoryKv, seed: u64) -> Self {
        Self {
            inner,
            seed,
            control: Mutex::new(Control::default()),
        }
    }
    pub fn arm(&self, plan: Failpoint) -> oneshot::Receiver<()> {
        assert!(plan.nth > 0);
        let mut c = self.control.lock().unwrap();
        assert_eq!(c.active, 0, "arm only after requests settle");
        let (tx, rx) = oneshot::channel();
        c.plan = Some(plan);
        c.seen = 0;
        c.signal = Some(tx);
        c.events.clear();
        c.max_active = 0;
        rx
    }
    pub fn disarm(&self) {
        let mut c = self.control.lock().unwrap();
        assert_eq!(c.active, 0, "requests must settle before recovery");
        c.plan = None;
        c.signal = None;
    }
    pub fn events(&self) -> Vec<Event> {
        self.control.lock().unwrap().events.clone()
    }
    pub fn active(&self) -> usize {
        self.control.lock().unwrap().active
    }
    pub fn max_active(&self) -> usize {
        self.control.lock().unwrap().max_active
    }

    async fn run(
        &self,
        operation: Operation,
        key: &str,
        value: Option<Vec<u8>>,
    ) -> io::Result<Option<Vec<u8>>> {
        let (plan, ordinal) = {
            let mut c = self.control.lock().unwrap();
            let matches = c
                .plan
                .as_ref()
                .is_some_and(|p| p.operation == operation && key.starts_with(&p.key_prefix));
            let plan = if matches {
                c.seen += 1;
                c.plan.as_ref().filter(|p| p.nth == c.seen).cloned()
            } else {
                None
            };
            c.events.push(Event {
                operation,
                key: key.into(),
                faulted: plan.is_some(),
            });
            c.active += 1;
            c.max_active = c.max_active.max(c.active);
            (plan, c.events.len() as u64)
        };
        let _active = Active(self);
        // Stable differing delays perturb concurrent completion order without a
        // wall clock, a runtime dependency, or sleeps.
        let hash = key.bytes().fold(self.seed.wrapping_add(ordinal), |n, b| {
            n.wrapping_mul(31).wrapping_add(b as u64)
        });
        for _ in 0..if self.seed == 0 { 0 } else { 1 + hash % 11 } {
            yield_once().await;
        }
        let mut result = None;
        match plan.as_ref().map(|p| p.effect) {
            Some(Effect::None) => {}
            Some(Effect::Visible) if operation != Operation::Get => match value {
                Some(v) => self.inner.simulate_unacknowledged_put(key, v),
                None => self.inner.simulate_unacknowledged_delete(key),
            },
            _ => match operation {
                Operation::Get => result = self.inner.get(key).await?,
                Operation::Put => {
                    self.inner
                        .put(key, value.expect("put requires value"))
                        .await?
                }
                Operation::Delete => self.inner.delete(key).await?,
            },
        }
        if let Some(plan) = plan {
            if let Some(tx) = self.control.lock().unwrap().signal.take() {
                let _ = tx.send(());
            }
            if plan.cancel {
                futures::future::pending::<()>().await;
            }
            return Err(io::Error::other(format!(
                "injected {operation:?} failure at {key}"
            )));
        }
        Ok(result)
    }
}

impl KvStore for FaultKv {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Vec<u8>>>> {
        Box::pin(self.run(Operation::Get, key, None))
    }
    fn put<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move { self.run(Operation::Put, key, Some(value)).await.map(|_| ()) })
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move { self.run(Operation::Delete, key, None).await.map(|_| ()) })
    }
}
