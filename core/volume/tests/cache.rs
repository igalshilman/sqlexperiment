//! The owner cache across owners, crashes and readers: no session reads a
//! page a checkpoint replaced, and a reader knows when it might have.
#![cfg(feature = "simulation")]
mod support;

use std::sync::Arc;
use support::*;
use volume::{NamespaceCache, Result, StorageOptions, kv::KvStore, simulation::*};

/// Options that checkpoint after every commit.
fn options(cache: Option<&NamespaceCache>) -> StorageOptions {
    StorageOptions {
        cache: cache.cloned(),
        checkpoint_after: 0,
        inline_log_bytes: 256,
        ..StorageOptions::default()
    }
}

fn failing_sync() -> Failpoint {
    Failpoint {
        operation: Operation::Sync,
        key_prefix: String::new(),
        nth: 1,
        effect: Effect::None,
        cancel: false,
    }
}

/// Commit `ops` in a new session; the model follows.
async fn commit(
    store: &Arc<dyn KvStore>,
    cache: Option<&NamespaceCache>,
    model: &mut Model,
    ops: &[Op],
) -> Result<()> {
    let session = Session::open(store.clone(), options(cache)).await?;
    assert_eq!(model.differs(&session).await?, None);
    for op in ops {
        session.apply(op)?;
    }
    session.volume.commit().await?;
    *model = model.applied(ops);
    Ok(())
}

/// Owner A's commit fails at its sync; A reopens and reads what the failed
/// checkpoint wrote (visible, not durable), caching its pages under the next
/// epoch. A crash loses it all. Another process, B, takes over and
/// checkpoints to that same epoch number with other pages. A, back with its
/// cache, must read B's pages, not the ones it cached.
#[test]
fn a_cache_that_read_a_lost_checkpoint_never_serves_it() -> Result<()> {
    futures::executor::block_on(async {
        let memory = MemoryKv::default();
        let fault = Arc::new(FaultKv::new(memory.clone(), 0));
        let store: Arc<dyn KvStore> = fault.clone();
        let (a, b) = (fresh_namespace(), fresh_namespace());
        let mut model = Model::default();
        commit(
            &store,
            Some(&a),
            &mut model,
            &[Op::Write("db", 0, Some(page(1)))],
        )
        .await?;

        // A's checkpointing commit fails at its sync: its writes are visible.
        let session = Session::open(store.clone(), options(Some(&a))).await?;
        session.apply(&Op::Write("db", 0, Some(page(2))))?;
        let _gate = fault.arm(failing_sync());
        assert!(session.volume.commit().await.is_err());
        fault.disarm();
        drop(session);
        // A reopens on them, and reads (and caches) the checkpointed page.
        let failed = model.applied(&[Op::Write("db", 0, Some(page(2)))]);
        let session = Session::open(store.clone(), options(Some(&a))).await?;
        assert_eq!(failed.differs(&session).await?, None);
        drop(session);
        // The backend loses every write since the last sync, if any remain.
        memory.crash();
        let session = Session::open(store.clone(), options(None)).await?;
        if failed.matches(&session).await? {
            model = failed;
        }
        drop(session);

        // B, another process, checkpoints to the same epoch with another page.
        commit(
            &store,
            Some(&b),
            &mut model,
            &[Op::Write("db", 0, Some(page(3)))],
        )
        .await?;
        // A reads B's page.
        let session = Session::open(store.clone(), options(Some(&a))).await?;
        assert_eq!(model.differs(&session).await?, None);
        Ok(())
    })
}

/// A reader beside the owner, which commits and checkpoints meanwhile:
/// whatever the reader read that is not the state it opened on, it learns
/// from `is_current`, and it never writes to the store.
#[test]
fn a_reader_beside_a_checkpointing_owner_knows_when_it_is_stale() -> Result<()> {
    futures::executor::block_on(async {
        let mut rng = Rng::new(0x7ead);
        let memory = MemoryKv::default();
        let store: Arc<dyn KvStore> = Arc::new(memory.clone());
        let owner = fresh_namespace();
        let mut model = Model::default();
        let (mut stale, mut current) = (0, 0);
        for round in 0..60 {
            let ops: Vec<Op> = (0..1 + rng.below(6))
                .map(|_| Op::random(&mut rng))
                .collect();
            commit(&store, Some(&owner), &mut model, &ops).await?;
            let lazy = StorageOptions {
                prefetch_pages: rng.below(2) * 16,
                ..StorageOptions::default()
            };
            let reader = Session::reader(store.clone(), lazy).await?;
            let opened = model.clone();
            let mut differed = false;
            let commits = rng.below(3);
            for step in 0..4 {
                if step < commits {
                    // Pages the reader reads often, and anything else.
                    let hot = Op::Write("db", rng.below(4), Some(varied(&mut rng)));
                    let ops = [hot, Op::random(&mut rng)];
                    commit(&store, Some(&owner), &mut model, &ops).await?;
                }
                for _ in 0..4 {
                    let (name, index) = match rng.percent(50) {
                        true => ("db", rng.below(4)),
                        false => {
                            let name = rng.pick(&FILES);
                            (name, rng.below(opened.len(name).div_ceil(4096) + 1))
                        }
                    };
                    let read = reader.file(name).read(index).await?;
                    differed |= read != opened.page(name, index);
                }
            }
            for name in RECORDS {
                assert_eq!(reader.volume.record(name).get(), opened.record(name));
            }
            let is_current = reader.volume.is_current().await?;
            assert!(
                !differed || !is_current,
                "round {round}: read a replaced page"
            );
            assert!(is_current || commits > 0, "round {round}");
            (stale, current) = match differed {
                true => (stale + 1, current),
                false => (stale, current + is_current as usize),
            };
            // Its changes stay its own.
            let keys = memory.keys();
            reader.apply(&Op::Set("progress", "reader".into()))?;
            assert!(reader.volume.commit().await.is_err());
            assert_eq!(memory.keys(), keys);
        }
        assert!(
            stale > 10 && current > 10,
            "{stale} stale, {current} current"
        );
        Ok(())
    })
}

/// Two sessions of one owner overlap: the open one reads the state it opened
/// on while the other commits, whose checkpoints wait until it ends; then it
/// may not commit on top.
#[test]
fn an_open_session_holds_back_checkpoints_and_reads_its_state() -> Result<()> {
    futures::executor::block_on(async {
        let memory = MemoryKv::default();
        let store: Arc<dyn KvStore> = Arc::new(memory.clone());
        let cache = fresh_namespace();
        let mut model = Model::default();
        let mut rng = Rng::new(0x0e7);
        for _ in 0..3 {
            let ops: Vec<Op> = (0..6).map(|_| Op::random(&mut rng)).collect();
            commit(&store, Some(&cache), &mut model, &ops).await?;
        }
        let open = Session::open(store.clone(), options(Some(&cache))).await?;
        let opened = model.clone();
        let writer = Session::open(store.clone(), options(Some(&cache))).await?;
        for n in 0..5 {
            let ops = [
                Op::Write("db", n, Some(page(100 + n))),
                Op::random(&mut rng),
            ];
            for op in &ops {
                writer.apply(op)?;
            }
            let report = writer.volume.commit().await?;
            assert_eq!(report.checkpointed_pages, 0, "waits for the open session");
            model = model.applied(&ops);
            assert_eq!(opened.differs(&open).await?, None);
        }
        open.apply(&Op::Set("progress", "late".into()))?;
        let error = open.volume.commit().await.unwrap_err().to_string();
        assert!(error.contains("another session committed"), "{error}");
        drop(open);
        writer.apply(&Op::Write("db", 0, Some(page(7))))?;
        model.apply(&Op::Write("db", 0, Some(page(7))));
        assert!(writer.volume.commit().await?.checkpointed_pages > 0);
        drop(writer);
        let session = Session::open(store, options(Some(&cache))).await?;
        assert_eq!(model.differs(&session).await?, None);
        Ok(())
    })
}
