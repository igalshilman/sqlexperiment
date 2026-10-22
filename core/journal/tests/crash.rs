//! A crash at every write of every commit of a sequence that sets records
//! (inline and stored), writes several files (one with a base), shrinks one,
//! seals the log, checkpoints and deletes garbage: each commit is atomic,
//! durable once its record is, and the journal goes on.
#![cfg(feature = "simulation")]
mod support;

use journal::{Base, NamespaceCache, Result, StorageOptions, kv::KvStore, simulation::*};
use std::sync::Arc;
use support::*;

const COMMIT: &str = "v15/commit";
/// Commits in the sequence.
const STEPS: u64 = 10;

fn options(cache: Option<NamespaceCache>) -> StorageOptions {
    StorageOptions {
        cache,
        checkpoint_after: 3,
        inline_log_bytes: 150,
        prefetch_pages: 0,
        hint_pages: 8,
        ..StorageOptions::default()
    }
}

/// The changes of commit `step`: small ones (an inline log, which seals once
/// it outgrows the budget), large ones (stored values), records over the
/// budget, holes, a shrink, and a page equal to its base's.
fn changes(step: u64) -> Vec<Op> {
    let mut rng = Rng::new(step);
    let image = |rng: &mut Rng| match step % 3 {
        0 => varied(rng),
        _ => page(step),
    };
    let mut ops = vec![
        Op::Set("progress", format!("step {step}").into_bytes().into()),
        Op::Write("db", step % 4, Some(image(&mut rng))),
        Op::Write("aux", 4 + step % 3, Some(image(&mut rng))),
        Op::Write("vm", step % 6, Some(image(&mut rng))),
    ];
    if step % 2 == 1 {
        ops.push(Op::Set("outbox", rng.bytes(600).into()));
    }
    if step % 3 == 2 {
        ops.push(Op::SetLen("aux", 512 * 9));
        let base = golden().page(1).map(journal::Page::copy_from_slice);
        ops.push(Op::Write("vm", 1, base));
    }
    if step % 4 == 3 {
        ops.push(Op::Write("db", 1, None));
    }
    ops
}

/// Open a session, read a few pages (for the working-set hint), and make
/// `ops` its pending changes.
async fn session(
    store: Arc<dyn KvStore>,
    cache: Option<NamespaceCache>,
    ops: &[Op],
) -> Result<Session> {
    let session = Session::open(store, options(cache)).await?;
    for name in FILES {
        session.file(name).read(0).await?;
    }
    for op in ops {
        session.apply(op)?;
    }
    Ok(session)
}

#[test]
fn a_commit_is_atomic_at_every_write_of_a_sequence() -> Result<()> {
    futures::executor::block_on(async {
        let durable = MemoryKv::default();
        let mut model = Model::default();
        let (mut cases, mut kinds) = (0, std::collections::BTreeSet::new());
        for step in 0..STEPS {
            let ops = changes(step);
            let new = model.applied(&ops);
            for cached in [false, true] {
                // The commit fails at its sync, every write queued.
                let fault = Arc::new(FaultKv::new(durable.fork(), 0));
                let cache = cached.then(fresh_namespace);
                let failing = session(fault.clone(), cache.clone(), &ops).await?;
                let _signal = fault.arm(Failpoint {
                    operation: Operation::Sync,
                    key_prefix: String::new(),
                    nth: 1,
                    effect: Effect::None,
                    cancel: false,
                });
                assert!(failing.journal.commit().await.is_err());
                fault.disarm();
                drop(failing);
                let writes = fault.inner.unsynced();
                let record = writes.iter().position(|(key, _)| key == COMMIT).unwrap();
                for (key, value) in &writes {
                    let kind = key.split('/').nth(1).unwrap_or_default();
                    kinds.insert(format!(
                        "{kind} {}",
                        if value.is_some() { "put" } else { "delete" }
                    ));
                }
                for kept in 0..=writes.len() {
                    cases += 1;
                    let memory = fault.inner.fork();
                    memory.crash_keeping(kept);
                    let store = Arc::new(
                        MonitorKv::seeded(memory.clone(), memory.values())
                            .with_base("vm", golden()),
                    );
                    // The owner's cache saw the failure; reading, it follows
                    // this crash's history as well as any.
                    let reopened = Session::open(store.clone(), options(cache.clone())).await?;
                    let expected = if kept > record { &new } else { &model };
                    let case = format!(
                        "step {step}, cached {cached}: {kept} of {} writes",
                        writes.len()
                    );
                    assert_eq!(expected.differs(&reopened).await?, None, "{case}");
                    drop(reopened);
                    // A next commit, by a new owner, survives a crash.
                    let next = [
                        Op::Set("state", "next".into()),
                        Op::Write("db", 9, Some(page(99))),
                    ];
                    let after = session(store.clone(), cached.then(fresh_namespace), &next).await?;
                    after.journal.commit().await?;
                    drop(after);
                    memory.crash();
                    let reopened = Session::open(store.clone(), options(None)).await?;
                    let next = expected.applied(&next);
                    assert_eq!(
                        next.differs(&reopened).await?,
                        None,
                        "{case}, then another commit"
                    );
                    assert_eq!(store.violations(), Vec::<String>::new(), "{case}");
                }
            }
            // The sequence goes on from the commit acknowledged.
            let store: Arc<dyn KvStore> = Arc::new(durable.clone());
            session(store, None, &ops).await?.journal.commit().await?;
            model = new;
        }
        // The sequence covers every kind of write a commit makes.
        let kinds: Vec<_> = kinds.into_iter().collect();
        let expected = [
            "commit put",
            "hint put",
            "log delete",
            "log put",
            "page delete",
            "page put",
        ];
        assert_eq!(kinds, expected);
        println!("crashed commits: {cases} cases");
        Ok(())
    })
}
