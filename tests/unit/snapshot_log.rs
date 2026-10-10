use super::*;
use crate::{
    io::BLOCK_SIZE,
    simulation::{Effect, Failpoint, FaultKv, MemoryKv, Operation},
};

fn page(n: u64) -> Vec<u8> {
    let mut p = vec![0; BLOCK_SIZE];
    p[..8].copy_from_slice(&n.to_le_bytes());
    p
}
fn logged(max_log_segments: u32) -> StorageOptions {
    StorageOptions {
        max_log_segments,
        ..StorageOptions::default()
    }
}
async fn open(store: &Arc<impl KvStore + 'static>, max: u32) -> io::Result<Snapshot> {
    Snapshot::open_with_options(store.clone(), logged(max)).await
}
async fn assert_no_orphans(store: &MemoryKv) -> io::Result<()> {
    let snapshot = Snapshot::open(Arc::new(store.clone())).await?;
    let mut expected = snapshot.reachable().await?;
    if snapshot.head_bytes.is_some() {
        expected.insert(HEAD.to_owned());
    }
    assert_eq!(store.keys(), expected, "untracked objects remain");
    Ok(())
}
fn log_keys(store: &MemoryKv) -> Vec<String> {
    let prefix = Kind::Log.prefix();
    store
        .keys()
        .into_iter()
        .filter(|k| k.starts_with(prefix))
        .collect()
}

#[test]
fn appends_commit_with_one_put_and_fold_at_the_limit() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        // Enabling the log on an empty store is a full publication.
        let pages: Changes = (0..8).map(|i| (i, Some(page(i)))).collect();
        open(&store, 3).await?.publish(pages, 8 * 4096).await?;
        assert_eq!(open(&store, 3).await?.head.log_start, 1);

        for n in 1..=3 {
            store.reset_stats();
            let snapshot = open(&store, 3).await?;
            assert_eq!(snapshot.log.count, n - 1);
            let opened = store.stats();
            assert_eq!(opened.gets, 2 + 4, "head, intent and one probe batch");
            assert_eq!(opened.puts, u64::from(n > 1), "confirm the last segment");
            store.reset_stats();
            let dirty = Changes::from([(0, Some(page(100 + n))), (8 + n, Some(page(n)))]);
            snapshot.publish(dirty, (9 + n) * 4096).await?;
            let saved = store.stats();
            assert_eq!((saved.puts, saved.deletes), (1, 0));
            assert_eq!(saved.write_keys.first(), Some(&Kind::Log.key(n)));
        }
        let snapshot = open(&store, 3).await?;
        assert_eq!(snapshot.size(), 12 * 4096);
        assert_eq!(snapshot.block(0).await?, Some(page(103)));
        assert_eq!(snapshot.block(10).await?, Some(page(2)));
        assert_eq!(snapshot.block(5).await?, Some(page(5)));

        // The fourth save folds the three segments into a new map.
        snapshot
            .publish(Changes::from([(1, Some(page(77)))]), 12 * 4096)
            .await?;
        assert!(log_keys(&store).is_empty());
        let snapshot = open(&store, 3).await?;
        assert_eq!(snapshot.head.log_start, 4);
        assert_eq!(snapshot.log.count, 0);
        assert_eq!(snapshot.block(0).await?, Some(page(103)));
        assert_eq!(snapshot.block(1).await?, Some(page(77)));
        assert_eq!(snapshot.block(11).await?, Some(page(3)));
        assert_no_orphans(&store).await
    })
}

#[test]
fn disabling_the_log_folds_it_and_stops_probing() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let pages: Changes = (0..4).map(|i| (i, Some(page(i)))).collect();
        open(&store, 4).await?.publish(pages, 4 * 4096).await?;
        for n in 0..2 {
            let dirty = Changes::from([(n, None)]);
            open(&store, 4).await?.publish(dirty, 4 * 4096).await?;
        }
        assert_eq!(log_keys(&store).len(), 2);

        // A read-only save leaves the log as it is.
        store.reset_stats();
        open(&store, 0)
            .await?
            .publish(Changes::new(), 4 * 4096)
            .await?;
        assert_eq!(store.stats().deletes, 0);
        assert_eq!(log_keys(&store).len(), 2);

        let snapshot = open(&store, 0).await?;
        assert_eq!(snapshot.block(0).await?, None, "logged hole");
        snapshot
            .publish(Changes::from([(3, Some(page(9)))]), 4 * 4096)
            .await?;
        assert!(log_keys(&store).is_empty());
        store.reset_stats();
        let snapshot = open(&store, 0).await?;
        assert_eq!(snapshot.head.log_start, 0);
        assert_eq!(store.stats().gets, 2, "no probes with the log disabled");
        assert_eq!(snapshot.block(1).await?, None);
        assert_eq!(snapshot.block(2).await?, Some(page(2)));
        assert_eq!(snapshot.block(3).await?, Some(page(9)));
        assert_no_orphans(&store).await
    })
}

#[test]
fn oversized_segments_fold_instead() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let raw = StorageOptions {
            page_compression: PageCompression::Raw,
            max_log_segments: 8,
            ..StorageOptions::default()
        };
        let pages: Changes = (0..2).map(|i| (i, Some(page(i)))).collect();
        open(&store, 8).await?.publish(pages, 2 * 4096).await?;
        // Raw pages larger than one segment can hold.
        let pages: Changes = (0..300).map(|i| (i, Some(page(i + 1)))).collect();
        Snapshot::open_with_options(store.clone(), raw)
            .await?
            .publish(pages, 300 * 4096)
            .await?;
        assert!(log_keys(&store).is_empty());
        let snapshot = open(&store, 8).await?;
        assert_eq!(snapshot.block(299).await?, Some(page(300)));
        assert_no_orphans(&store).await
    })
}

#[test]
fn open_confirms_a_visible_segment_and_rejects_gaps() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        open(&store, 4)
            .await?
            .publish(Changes::from([(0, Some(page(1)))]), 4096)
            .await?;
        // A failed append left a visible but not durable segment.
        let segment = log::encode(
            1,
            4096,
            &Changes::from([(0, Some(page(2)))]),
            PageCompression::Lz4,
        )?;
        store.simulate_unacknowledged_put(&Kind::Log.key(1), segment.clone());
        assert_eq!(open(&store, 4).await?.block(0).await?, Some(page(2)));
        store.crash(&[]);
        assert_eq!(
            open(&store, 4).await?.block(0).await?,
            Some(page(2)),
            "open must make what it exposes durable"
        );

        store.put(&Kind::Log.key(3), segment).await?;
        assert!(open(&store, 4).await.is_err(), "gap in the log");
        Ok(())
    })
}

/// Fail every mutation of an append and of a fold, in every outcome, with and
/// without a crash. The save is atomic, the next save works and nothing leaks.
#[test]
fn interrupted_appends_and_folds_are_atomic() -> io::Result<()> {
    futures::executor::block_on(async {
        let mut cases = 0;
        for logged_before in 0..=2 {
            let base = MemoryKv::default();
            let base_store = Arc::new(base.clone());
            let pages: Changes = (0..8).map(|i| (i, Some(page(i)))).collect();
            open(&base_store, 2).await?.publish(pages, 8 * 4096).await?;
            for n in 0..logged_before {
                let dirty = Changes::from([(0, Some(page(50 + n)))]);
                open(&base_store, 2).await?.publish(dirty, 8 * 4096).await?;
            }
            let old = open(&base_store, 2).await?.block(0).await?;
            for operation in [Operation::Put, Operation::Delete] {
                'ordinal: for nth in 1.. {
                    for effect in [Effect::None, Effect::Visible, Effect::After] {
                        for crash in [false, true] {
                            let store = Arc::new(FaultKv::new(base.fork(), 0));
                            let snapshot = open(&store, 2).await?;
                            let _gate = store.arm(Failpoint {
                                operation,
                                key_prefix: String::new(),
                                nth,
                                effect,
                                cancel: false,
                            });
                            let dirty = Changes::from([(0, Some(page(100))), (9, Some(page(9)))]);
                            let result = snapshot.publish(dirty, 10 * 4096).await;
                            let faulted = store.events().iter().any(|e| e.faulted);
                            store.disarm();
                            if !faulted {
                                result?;
                                break 'ordinal;
                            }
                            cases += 1;
                            if crash {
                                store.inner.crash(&[]);
                            }
                            let reopened = open(&store, 2).await?;
                            let (zero, nine) = (reopened.block(0).await?, reopened.block(9).await?);
                            if result.is_ok() || zero == Some(page(100)) {
                                assert_eq!((zero, nine), (Some(page(100)), Some(page(9))));
                            } else {
                                assert_eq!((zero, nine), (old.clone(), None));
                            }
                            let size = reopened.size();
                            reopened
                                .publish(Changes::from([(2, Some(page(7)))]), size)
                                .await?;
                            // The follow-up commit survives a crash, so it never built on
                            // a segment that was visible without being durable.
                            store.inner.crash(&[]);
                            let reopened = open(&store, 2).await?;
                            assert_eq!(reopened.block(2).await?, Some(page(7)));
                            assert_no_orphans(&store.inner).await?;
                        }
                    }
                }
            }
        }
        assert!(cases > 40, "{cases}");
        println!("interrupted log saves: {cases} cases");
        Ok(())
    })
}

#[test]
fn cached_log_start_probes_with_the_head_and_survives_stale_hints() -> io::Result<()> {
    futures::executor::block_on(async {
        use crate::{cache::ObjectCache, simulation::LatencyKv};
        let store = Arc::new(LatencyKv::new(MemoryKv::default()));
        let cache = ObjectCache::new(1 << 20);
        let cached = StorageOptions {
            cache: Some(cache.clone()),
            max_log_segments: 4,
            ..StorageOptions::default()
        };
        let open_cached = || Snapshot::open_with_options(store.clone(), cached.clone());
        open_cached()
            .await?
            .publish(Changes::from([(0, Some(page(1)))]), 4096)
            .await?;
        open_cached()
            .await?
            .publish(Changes::from([(0, Some(page(2)))]), 4096)
            .await?;
        store.reset();
        let snapshot = open_cached().await?;
        assert_eq!(
            store.round_trips(),
            2,
            "head, intent and probe; then confirm"
        );
        assert_eq!(snapshot.block(0).await?, Some(page(2)));

        // A writer without this cache folds and disables the log, then re-enables
        // it at a different start: the remembered start is stale twice over.
        open(&store, 0)
            .await?
            .publish(Changes::from([(0, Some(page(3)))]), 4096)
            .await?;
        assert_eq!(cache.log_start(), 1, "stale hint");
        assert_eq!(open_cached().await?.block(0).await?, Some(page(3)));
        open(&store, 4)
            .await?
            .publish(Changes::from([(0, Some(page(4)))]), 4096)
            .await?;
        open(&store, 4)
            .await?
            .publish(Changes::from([(0, Some(page(5)))]), 4096)
            .await?;
        cache.set_log_start(9);
        assert_eq!(open_cached().await?.block(0).await?, Some(page(5)));
        assert_eq!(cache.log_start(), 1);
        Ok(())
    })
}
