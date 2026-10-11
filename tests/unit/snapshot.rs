use super::*;
use crate::{
    format::{LOG_PREFIX, PAGE_PREFIX},
    io::BLOCK_SIZE,
    simulation::{Effect, Failpoint, FaultKv, LatencyKv, MemoryKv, Operation},
};

/// First eight bytes of a page as its number, for readable assertions.
fn number(page: Option<Page>) -> Option<u64> {
    page.map(|p| u64::from_le_bytes(p[..8].try_into().unwrap()))
}
fn page(n: u64) -> Page {
    let mut p = vec![0; BLOCK_SIZE];
    p[..8].copy_from_slice(&n.to_le_bytes());
    p.into()
}
fn options(checkpoint_after: u32, cache: Option<&PageCache>) -> StorageOptions {
    StorageOptions {
        checkpoint_after,
        cache: cache.cloned(),
        ..StorageOptions::default()
    }
}
async fn open<S: KvStore + 'static>(
    store: &Arc<S>,
    options: &StorageOptions,
) -> io::Result<Snapshot> {
    Snapshot::open_with_options(store.clone(), options.clone()).await
}

/// The expected database: pages and size.
#[derive(Clone, Default)]
struct Model {
    pages: Changes,
    size: u64,
}
impl Model {
    fn apply(&mut self, dirty: &Changes, size: u64) {
        self.pages.extend(dirty.clone());
        self.size = size;
    }
    async fn check(&self, snapshot: &Snapshot) -> io::Result<bool> {
        if snapshot.size() != self.size {
            return Ok(false);
        }
        for index in 0..self.size / BLOCK_SIZE as u64 {
            let expected = self.pages.get(&index).cloned().flatten();
            if snapshot.block(index).await? != expected {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
fn dirty(round: u64) -> (Changes, u64) {
    let changes = Changes::from([
        (0, Some(page(1000 + round))),
        (round % 5 + 1, Some(page(round))),
    ]);
    (changes, 8 * 4096)
}
fn keys(store: &MemoryKv, prefix: &str) -> Vec<String> {
    store
        .keys()
        .into_iter()
        .filter(|k| k.starts_with(prefix))
        .collect()
}
/// No log segment below `meta.deleted_below` survives.
async fn assert_collected(store: &MemoryKv) -> io::Result<()> {
    let meta = match store.get(META).await? {
        Some(bytes) => Meta::decode(&bytes)?,
        None => Meta::default(),
    };
    for key in keys(store, LOG_PREFIX) {
        let seq = u64::from_str_radix(key.trim_start_matches(LOG_PREFIX), 16).unwrap();
        assert!(seq >= meta.deleted_below, "{key} below {meta:?}");
    }
    Ok(())
}

#[test]
fn a_save_is_one_put_and_a_checkpointed_page_one_get() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let keep = options(4, None);
        let changes: Changes = (0..3).map(|i| (i, Some(page(i)))).collect();
        open(&store, &keep)
            .await?
            .publish(changes.clone(), 3 * 4096)
            .await?;
        assert_eq!(store.stats().puts, 1);
        assert_eq!(store.keys().into_iter().collect::<Vec<_>>(), [log_key(1)]);

        // Checkpointing everything moves pages to their keys and advances meta.
        let all = options(0, None);
        let snapshot = open(&store, &all).await?;
        let report = snapshot
            .publish(Changes::from([(3, None)]), 4 * 4096)
            .await?;
        assert_eq!(report.checkpointed_pages, 3);
        assert_eq!(keys(&store, PAGE_PREFIX).len(), 3);
        let meta = Meta::decode(&store.get(META).await?.unwrap())?;
        assert_eq!((meta.log_start, meta.size), (2, 3 * 4096));

        let snapshot = open(&store, &all).await?;
        store.reset_stats();
        assert_eq!(number(snapshot.block(1).await?), Some(1));
        assert_eq!(store.stats().gets, 1, "no map: one get per page");
        assert_eq!(number(snapshot.block(3).await?), None);
        assert_eq!(snapshot.size(), 4 * 4096);
        Ok(())
    })
}

/// Without a cache, saves cost one round trip, or two when they checkpoint.
#[test]
fn checkpoints_follow_the_log_without_a_cache() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(MemoryKv::default()));
        let opts = options(2, None);
        let mut model = Model::default();
        let mut round_trips = Vec::new();
        for round in 0..16 {
            let snapshot = open(&store, &opts).await?;
            assert!(model.check(&snapshot).await?, "round {round}");
            let (changes, size) = dirty(round);
            store.reset();
            store.inner.reset_stats();
            let report = snapshot.publish(changes.clone(), size).await?;
            assert!(report.cleanup_error.is_none());
            round_trips.push(store.round_trips());
            model.apply(&changes, size);
            // The log plus garbage awaiting the next checkpoint stays bounded.
            assert!(keys(&store.inner, LOG_PREFIX).len() <= 7);
            assert_collected(&store.inner).await?;
        }
        assert!(
            round_trips.iter().all(|&rt| rt == 1 || rt == 2),
            "{round_trips:?}"
        );
        let checkpoints = round_trips.iter().filter(|&&rt| rt == 2).count();
        assert!((3..=6).contains(&checkpoints), "{round_trips:?}");
        assert!(model.check(&open(&store, &opts).await?).await?);
        Ok(())
    })
}

/// With an owner cache, every steady-state open and save is one round trip, even
/// on a backend that needs confirmation, and a restarted owner still recovers.
#[test]
fn an_owner_cache_makes_opens_and_saves_one_round_trip() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(MemoryKv::default()));
        let cache = PageCache::new(1 << 20);
        let opts = options(2, Some(&cache));
        let mut model = Model::default();
        for round in 0..16 {
            store.reset();
            store.inner.reset_stats();
            let snapshot = open(&store, &opts).await?;
            if round > 0 {
                assert_eq!(store.round_trips(), 1, "open {round}");
                assert_eq!(store.inner.stats().puts, 0, "no confirmation needed");
            }
            assert!(model.check(&snapshot).await?, "round {round}");
            let (changes, size) = dirty(round);
            store.reset();
            snapshot.publish(changes.clone(), size).await?;
            assert_eq!(store.round_trips(), 1, "save {round}");
            model.apply(&changes, size);
            assert_collected(&store.inner).await?;
        }
        assert!(cache.bytes() > 0);
        // A restarted owner starts from the durable meta and replays further.
        let restarted = options(2, Some(&PageCache::new(1 << 20)));
        assert!(model.check(&open(&store, &restarted).await?).await?);
        assert!(model.check(&open(&store, &options(2, None)).await?).await?);
        Ok(())
    })
}

#[test]
fn another_writer_resets_or_is_overridden_in_the_cache() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(FaultKv::new(MemoryKv::default(), 0));
        let cache = PageCache::new(1 << 20);
        let owner = options(0, Some(&cache));
        let other = options(0, None);
        let mut model = Model::default();
        for round in 0..4 {
            let (changes, size) = dirty(round);
            open(&store, &owner)
                .await?
                .publish(changes.clone(), size)
                .await?;
            model.apply(&changes, size);
        }
        let snapshot = open(&store, &owner).await?;
        assert!(model.check(&snapshot).await?);
        assert!(cache.bytes() > 0, "pages are cached");

        // Another writer checkpoints: meta changes and the cache resets.
        for round in 10..13 {
            let (changes, size) = dirty(round);
            open(&store, &other)
                .await?
                .publish(changes.clone(), size)
                .await?;
            model.apply(&changes, size);
        }
        assert!(model.check(&open(&store, &owner).await?).await?);

        // It rewrites pages in place but fails to record meta: the cache keeps
        // stale pages, and the log still overrides every one of them.
        let snapshot = open(&store, &owner).await?;
        assert!(model.check(&snapshot).await?);
        for round in 20..22 {
            let (changes, size) = dirty(round);
            let snapshot = open(&store, &other).await?;
            let _gate = store.arm(Failpoint {
                operation: Operation::Put,
                key_prefix: META.into(),
                nth: 1,
                effect: Effect::None,
                cancel: false,
            });
            let report = snapshot.publish(changes.clone(), size).await?;
            store.disarm();
            assert!(report.cleanup_error.is_some() || round == 20);
            model.apply(&changes, size);
        }
        assert!(model.check(&open(&store, &owner).await?).await?);
        Ok(())
    })
}

#[test]
fn weak_backends_confirm_what_this_owner_did_not_write() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let opts = options(4, None);
        open(&store, &opts)
            .await?
            .publish(Changes::from([(0, Some(page(1)))]), 4096)
            .await?;
        // A failed append is visible but not durable.
        let images = Images::from([(
            0,
            Some(Page::from(page_codec::encode(
                &page(2),
                PageCompression::Raw,
            )?)),
        )]);
        let segment = log::encode_transaction(2, 4096, &images).remove(0);
        store.simulate_unacknowledged_put(&log_key(2), segment);
        assert_eq!(number(open(&store, &opts).await?.block(0).await?), Some(2));
        store.crash(&[]);
        assert_eq!(
            number(open(&store, &opts).await?.block(0).await?),
            Some(2),
            "open must make what it exposes durable"
        );
        // A visible-only meta is confirmed too. This one only claims that the
        // checkpointed segments were deleted, which a failed save could write.
        let meta = Meta {
            log_start: 3,
            deleted_below: 3,
            size: 4096,
        };
        open(&store, &options(0, None))
            .await?
            .publish(Changes::from([(1, Some(page(9)))]), 2 * 4096)
            .await?;
        let durable = store.get(META).await?.unwrap();
        assert_eq!(
            Meta::decode(&durable)?,
            Meta {
                deleted_below: 1,
                ..meta
            }
        );
        store.simulate_unacknowledged_put(META, meta.encode());
        drop(open(&store, &opts).await?);
        store.crash(&[]);
        assert_ne!(store.get(META).await?.unwrap(), durable);

        // A backend whose visible values are durable needs no confirmation.
        let strong = Arc::new(LatencyKv::new((*store).clone()).with_durable_when_visible(true));
        store.reset_stats();
        let snapshot = open(&strong, &opts).await?;
        assert_eq!(store.stats().puts, 0);
        assert_eq!(number(snapshot.block(1).await?), Some(9));
        Ok(())
    })
}

#[test]
fn large_transactions_span_segments_and_failed_ones_leave_only_debris() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(FaultKv::new(MemoryKv::default(), 3));
        let raw = StorageOptions {
            page_compression: PageCompression::Raw,
            ..options(4, None)
        };
        let big = |n: u64| -> Changes { (0..300).map(|i| (i, Some(page(i + n)))).collect() };
        open(&store, &raw)
            .await?
            .publish(big(0), 300 * 4096)
            .await?;
        assert_eq!(keys(&store.inner, LOG_PREFIX).len(), 2);

        // The commit part of the next one fails: its first part is debris.
        let snapshot = open(&store, &raw).await?;
        let _gate = store.arm(Failpoint {
            operation: Operation::Put,
            key_prefix: log_key(4),
            nth: 1,
            effect: Effect::None,
            cancel: false,
        });
        assert!(snapshot.publish(big(1), 300 * 4096).await.is_err());
        store.disarm();
        let snapshot = open(&store, &raw).await?;
        assert_eq!(number(snapshot.block(299).await?), Some(299));
        snapshot
            .publish(Changes::from([(5, Some(page(77)))]), 300 * 4096)
            .await?;
        let snapshot = open(&store, &raw).await?;
        assert_eq!(number(snapshot.block(5).await?), Some(77));
        assert_eq!(number(snapshot.block(6).await?), Some(6));
        Ok(())
    })
}

/// Build a log after `saves` saves with `checkpoint_after = 1`. After 3 saves
/// the next save only appends; after 4 it also checkpoints, collects garbage and
/// writes meta (a claim, with a cache).
async fn prepared(cache: Option<&PageCache>, saves: u64) -> io::Result<(MemoryKv, Model)> {
    let store = MemoryKv::default();
    let kv = Arc::new(store.clone());
    let opts = options(1, cache);
    let mut model = Model::default();
    for round in 0..saves {
        let (changes, size) = dirty(round);
        open(&kv, &opts)
            .await?
            .publish(changes.clone(), size)
            .await?;
        model.apply(&changes, size);
    }
    Ok((store, model))
}

/// Fail every mutation of a save in every outcome, with and without a crash
/// (a crash also restarts the owner and loses its cache). The save is atomic,
/// later saves work, nothing below the collected log survives.
#[test]
fn every_save_mutation_is_crash_safe() -> io::Result<()> {
    futures::executor::block_on(async {
        let mut cases = 0;
        // (cache, prepared saves, large faulted save)
        let scenarios = [
            (false, 3, false),
            (false, 4, false),
            (true, 3, false),
            (true, 4, false),
            (false, 4, true),
            (true, 4, true),
        ];
        for (cached, saves, large) in scenarios {
            for operation in [Operation::Put, Operation::Delete] {
                'ordinal: for nth in 1.. {
                    for effect in [Effect::None, Effect::Visible, Effect::After] {
                        for crash in [false, true] {
                            let cache = cached.then(|| PageCache::new(1 << 20));
                            let (base, old) = prepared(cache.as_ref(), saves).await?;
                            let store = Arc::new(FaultKv::new(base, 0));
                            let opts = StorageOptions {
                                page_compression: PageCompression::Raw,
                                ..options(1, cache.as_ref())
                            };
                            let snapshot = open(&store, &opts).await?;
                            let _gate = store.arm(Failpoint {
                                operation,
                                key_prefix: String::new(),
                                nth,
                                effect,
                                cancel: false,
                            });
                            // A large save checkpoints itself after committing.
                            let (changes, size) = if large {
                                let pages = (0..70).map(|i| (i, Some(page(500 + i))));
                                (pages.collect(), 70 * 4096)
                            } else {
                                dirty(50)
                            };
                            let result = snapshot.publish(changes.clone(), size).await;
                            let faulted = store.events().iter().any(|e| e.faulted);
                            store.disarm();
                            if !faulted {
                                result?;
                                break 'ordinal;
                            }
                            cases += 1;
                            let mut new = old.clone();
                            new.apply(&changes, size);
                            let mut opts = opts.clone();
                            if crash {
                                store.inner.crash(&[]);
                                opts.cache = cached.then(|| PageCache::new(1 << 20));
                            }
                            let reopened = open(&store, &opts).await?;
                            let committed = new.check(&reopened).await?;
                            assert!(committed || old.check(&reopened).await?);
                            if result.is_ok() {
                                assert!(committed, "acknowledged commit was lost");
                            }
                            let mut model = if committed { new } else { old };
                            let (changes, size) = dirty(51);
                            reopened.publish(changes.clone(), size).await?;
                            model.apply(&changes, size);
                            // The follow-up commit survives a crash.
                            store.inner.crash(&[]);
                            opts.cache = cached.then(|| PageCache::new(1 << 20));
                            assert!(model.check(&open(&store, &opts).await?).await?);
                            assert_collected(&store.inner).await?;
                        }
                    }
                }
            }
        }
        assert!(cases > 600, "{cases}");
        println!("interrupted saves: {cases} cases");
        Ok(())
    })
}
