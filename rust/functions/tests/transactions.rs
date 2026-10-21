//! A turn's transaction: participants prepare their parts, the host seals
//! them in one put, and tells them; a part that fails to prepare abandons
//! the turn, which runs again from the last seal.
mod common;

use anyhow::{Result, bail};
use async_trait::async_trait;
use common::{Store, attempt, programs, settle};
use functions::{Attached, Extension, Extensions, Outcome, Participant, Run, ScopeBlobs, Sink};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// An extension that counts the seals it takes part in, and logs what the
/// host tells it.
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

struct Counting {
    counter: Counter,
    count: u64,
}

#[async_trait]
impl Extension for Counter {
    async fn attach(&self, _blobs: ScopeBlobs, saved: Option<Value>) -> Result<Box<dyn Attached>> {
        let count = saved.and_then(|saved| saved["count"].as_u64());
        Ok(Box::new(Counting {
            counter: self.clone(),
            count: count.unwrap_or(0),
        }))
    }
}

#[async_trait]
impl Participant for Counting {
    async fn prepare(&mut self, sink: &mut Sink) -> Result<()> {
        if self.counter.fail.swap(false, Ordering::SeqCst) {
            bail!("Injected prepare failure");
        }
        self.count += 1;
        sink.record(json!({"count": self.count}));
        Ok(())
    }

    fn committed(&mut self) {
        let mut log = self.counter.log.lock().unwrap();
        log.push(format!("committed {}", self.count));
    }

    fn aborted(&mut self) {
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
async fn each_seal_holds_every_part_and_the_next_starts_from_it() {
    let counter = Counter::default();
    let store = Store::default();
    let catalog = programs(TWO_SLEEPS);
    let mut run = attempt(&store, &catalog, Value::Null, &offered(&counter)).await;
    assert_eq!(settle(&mut run).await, json!("done"));
    assert_eq!(counter.log(), ["committed 1", "committed 2", "committed 3"]);
    let seal = store.commit();
    assert_eq!(seal["seq"], 3);
    let parts: Vec<&String> = seal["parts"].as_object().unwrap().keys().collect();
    assert_eq!(parts, ["counter", "memory", "run"]);
    assert_eq!(seal["parts"]["counter"], json!({"count": 3}));

    // The next run in the scope: the extension's part carries over, as the
    // scope's tables do.
    let scope = store.scope().extended(offered(&counter));
    let mut next = Run::start(&catalog, &scope, "s", "r2", "v1", "main", Value::Null, 20)
        .await
        .unwrap();
    settle(&mut next).await;
    assert_eq!(store.commit()["seq"], 6);
    assert_eq!(store.commit()["parts"]["counter"], json!({"count": 6}));
}

#[tokio::test]
async fn a_part_that_fails_to_prepare_abandons_the_turn() {
    let counter = Counter::default();
    let store = Store::default();
    let catalog = programs(TWO_SLEEPS);
    let mut run = attempt(&store, &catalog, Value::Null, &offered(&counter)).await;
    counter.log();
    let sealed = store.commit();
    let timer = run.view().state.pending[0].clone();

    // The second turn's prepare fails: nothing is sealed, every participant
    // hears it, and the attempt is over.
    counter.fail.store(true, Ordering::SeqCst);
    let delivered = run
        .deliver(timer.id(), Outcome::success(Value::Null), 10)
        .await;
    assert!(format!("{:#}", delivered.unwrap_err()).contains("Injected prepare failure"));
    assert_eq!(counter.log(), ["aborted"]);
    assert_eq!(store.commit(), sealed);
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
