//! The owner cache across owners, crashes and readers: no session reads a
//! blob of a flush that was lost, and a reader reads the state it opened
//! while the owner flushes.
#![cfg(feature = "simulation")]
mod support;

use state::{Result, StateOptions, kv::KvStore, simulation::*};
use std::sync::Arc;
use support::*;

/// Small chunks, rebased often, and large records in blobs.
fn options(cache: Option<&Namespace>) -> Setup {
    Setup {
        options: StateOptions {
            chunk_pages: 2,
            rebase_ratio: 0.1,
            inline_record_bytes: 256,
            ..StateOptions::default()
        },
        cache: cache.cloned(),
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

/// Flush `ops` in a new session; the model follows.
async fn flush(
    store: &Arc<dyn KvStore>,
    cache: Option<&Namespace>,
    model: &mut Model,
    ops: &[Op],
) -> Result<()> {
    let session = Session::open(store.clone(), options(cache)).await?;
    assert_eq!(model.differs(&session).await?, None);
    session.flush(ops).await?;
    *model = model.applied(ops);
    Ok(())
}

/// Owner A's flush fails at its sync; A reopens and reads what the failed
/// flush wrote (visible, but synced by that open). A crash loses whatever is
/// not durable. Another process, B, takes over and flushes, at blob numbers
/// the lost flush may have used. A, back with its cache, must read B's
/// pages, not blobs it cached.
#[test]
fn a_cache_that_read_a_lost_checkpoint_never_serves_it() -> Result<()> {
    futures::executor::block_on(async {
        let memory = MemoryKv::default();
        let fault = Arc::new(FaultKv::new(memory.clone(), 0));
        let store: Arc<dyn KvStore> = fault.clone();
        let (a, b) = (fresh_namespace(), fresh_namespace());
        let mut model = Model::default();
        flush(
            &store,
            Some(&a),
            &mut model,
            &[Op::Write("db", 0, Some(page(1)))],
        )
        .await?;

        // A's flush fails at its sync: its writes are visible.
        let session = Session::open(store.clone(), options(Some(&a))).await?;
        let _gate = fault.arm(failing_sync());
        let write = [Op::Write("db", 0, Some(page(2)))];
        assert!(session.flush(&write).await.is_err());
        fault.disarm();
        drop(session);
        // A reopens on them, and reads (and caches) the page.
        let failed = model.applied(&write);
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

        // B, another process, flushes another page.
        flush(
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

/// A reader beside the owner, which flushes meanwhile: it reads the state
/// it opened, and never writes to the store; once the owner deleted a blob
/// it needs, a read of it fails, never reads another state.
#[test]
fn a_reader_beside_a_flushing_owner_reads_what_it_opened() -> Result<()> {
    futures::executor::block_on(async {
        let mut rng = Rng::new(0x7ead);
        let memory = MemoryKv::default();
        let store: Arc<dyn KvStore> = Arc::new(memory.clone());
        let owner = fresh_namespace();
        let mut model = Model::default();
        let grace = StateOptions::default().garbage_after;
        let (mut read, mut gone) = (0, 0);
        for round in 0..60 {
            let ops: Vec<Op> = (0..1 + rng.below(6))
                .map(|_| Op::random(&mut rng))
                .collect();
            flush(&store, Some(&owner), &mut model, &ops).await?;
            let lazy = Setup {
                options: StateOptions {
                    prefetch_pages: rng.below(2) * 16,
                    ..StateOptions::default()
                },
                cache: rng.percent(50).then(|| owner.clone()),
            };
            let reader = Session::reader(store.clone(), lazy).await?;
            let opened = model.clone();
            // Past the grace sometimes: then blobs it reads may be gone.
            let flushes = rng.pick(&[0, 1, 3, grace - 1, grace + 4]);
            for _ in 0..flushes {
                // Pages the reader reads often, and anything else.
                let hot = Op::Write("db", rng.below(4), Some(varied(&mut rng)));
                let ops = [hot, Op::random(&mut rng)];
                flush(&store, Some(&owner), &mut model, &ops).await?;
            }
            for _ in 0..16 {
                let name = rng.pick(&FILES);
                let index = rng.below(opened.len(name).div_ceil(4096) + 1);
                match reader.file(name).read(index).await {
                    Ok(page) => {
                        assert_eq!(page, opened.page(name, index), "round {round}");
                        read += 1;
                    }
                    Err(error) if flushes >= grace => {
                        assert!(error.to_string().contains("is gone"), "{error}");
                        gone += 1;
                    }
                    Err(error) => panic!("round {round}: {error}"),
                }
            }
            for name in RECORDS {
                assert_eq!(reader.state.record(name).get(), opened.record(name));
            }
            // Its WALs never flush.
            let keys = memory.keys();
            let error = reader.flush(&[Op::Set("progress", "reader".into())]).await;
            let error = error.unwrap_err().to_string();
            assert!(error.contains("a reader writes nothing"), "{error}");
            assert_eq!(memory.keys(), keys);
        }
        assert!(read > 100 && gone > 0, "{read} read, {gone} gone");
        Ok(())
    })
}

/// Two sessions of one owner overlap: the open one reads the state it opened
/// on while the other flushes; then it may not flush on top.
#[test]
fn an_open_session_reads_its_state_while_another_flushes() -> Result<()> {
    futures::executor::block_on(async {
        let memory = MemoryKv::default();
        let store: Arc<dyn KvStore> = Arc::new(memory.clone());
        let cache = fresh_namespace();
        let mut model = Model::default();
        let mut rng = Rng::new(0x0e7);
        for _ in 0..3 {
            let ops: Vec<Op> = (0..6).map(|_| Op::random(&mut rng)).collect();
            flush(&store, Some(&cache), &mut model, &ops).await?;
        }
        let open = Session::open(store.clone(), options(Some(&cache))).await?;
        let opened = model.clone();
        let writer = Session::open(store.clone(), options(Some(&cache))).await?;
        for n in 0..5 {
            let ops = [
                Op::Write("db", n, Some(page(100 + n))),
                Op::random(&mut rng),
            ];
            writer.flush(&ops).await?;
            model = model.applied(&ops);
            assert_eq!(opened.differs(&open).await?, None);
        }
        let late = [Op::Set("progress", "late".into())];
        let error = open.flush(&late).await.unwrap_err().to_string();
        assert!(error.contains("another state flushed"), "{error}");
        drop(open);
        let ops = [Op::Write("db", 0, Some(page(7)))];
        writer.flush(&ops).await?;
        model = model.applied(&ops);
        drop(writer);
        let session = Session::open(store, options(Some(&cache))).await?;
        assert_eq!(model.differs(&session).await?, None);
        Ok(())
    })
}
