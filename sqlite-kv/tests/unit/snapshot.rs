use super::*;
use crate::{
    PageCacheConfig,
    format::{HINT, LOG_PREFIX, PAGE_PREFIX},
    io::BLOCK_SIZE,
    simulation::{
        Effect, Failpoint, FaultKv, LatencyKv, MemoryKv, MonitorKv, Operation, cache_config,
        fresh_namespace, open_cache,
    },
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
fn options(checkpoint_after: u32, cache: Option<&NamespaceCache>) -> StorageOptions {
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
        let all = StorageOptions {
            prefetch_pages: 0,
            ..options(0, None)
        };
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
        let cache = fresh_namespace();
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
        // A restarted owner starts from the durable meta and replays further.
        let restarted = options(2, Some(&fresh_namespace()));
        assert!(model.check(&open(&store, &restarted).await?).await?);
        assert!(model.check(&open(&store, &options(2, None)).await?).await?);
        Ok(())
    })
}

#[test]
fn another_writer_is_refreshed_or_overridden_in_the_cache() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(FaultKv::new(MemoryKv::default(), 0));
        let cache = fresh_namespace();
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

        // Another writer checkpoints: meta changes and the cache is refreshed.
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
        let store = Arc::new(MonitorKv::new(FaultKv::new(MemoryKv::default(), 3)));
        let raw = StorageOptions {
            page_compression: PageCompression::Raw,
            ..options(4, None)
        };
        let big = |n: u64| -> Changes { (0..300).map(|i| (i, Some(page(i + n)))).collect() };
        open(&store, &raw)
            .await?
            .publish(big(0), 300 * 4096)
            .await?;
        assert_eq!(keys(&store.inner.inner, LOG_PREFIX).len(), 2);

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
        assert_eq!(store.violations(), Vec::<String>::new());
        Ok(())
    })
}

/// Build a log after `saves` saves with `checkpoint_after = 1`. After 3 saves
/// the next save only appends; after 4 it also checkpoints, collects garbage and
/// writes meta (a claim, with a cache).
async fn prepared(cache: Option<&NamespaceCache>, saves: u64) -> io::Result<(MemoryKv, Model)> {
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
        let (mut cases, mut checked) = (0, 0);
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
                            let cache = cached.then(fresh_namespace);
                            let (base, old) = prepared(cache.as_ref(), saves).await?;
                            let seed = base.durable_values();
                            let store = Arc::new(MonitorKv::seeded(FaultKv::new(base, 0), seed));
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
                                store.inner.inner.crash(&[]);
                                opts.cache = cached.then(fresh_namespace);
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
                            store.inner.inner.crash(&[]);
                            opts.cache = cached.then(fresh_namespace);
                            assert!(model.check(&open(&store, &opts).await?).await?);
                            assert_collected(&store.inner.inner).await?;
                            assert_eq!(store.violations(), Vec::<String>::new());
                            checked += store.checked();
                        }
                    }
                }
            }
        }
        assert!(cases > 600, "{cases}");
        println!("interrupted saves: {cases} cases, {checked} requests checked by the monitor");
        Ok(())
    })
}

/// Two owner processes, each with its own cache, hand the namespace back and
/// forth after random tenures. Saves fail in random places and outcomes, the
/// backend sometimes loses unconfirmed values, and owners sometimes restart.
/// Every open must read the model exactly, from whatever its cache kept.
#[test]
fn owners_handing_over_never_read_stale_pages() -> io::Result<()> {
    futures::executor::block_on(async {
        let mut seed = 0x5eed_u64;
        let mut random = |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let store = Arc::new(MonitorKv::new(FaultKv::new(MemoryKv::default(), 0)));
        let decoded = open_cache(PageCacheConfig {
            decoded: true,
            ..cache_config(8 * 4096, 64 << 20)
        });
        let mut caches = [fresh_namespace(), decoded.namespace("decoded")];
        let mut model = Model::default();
        let (mut kept, mut dropped, mut round) = (0, 0, 0);
        for tenure in 0..400 {
            let owner = tenure % 2;
            let opts = options(random(4) as u32, Some(&caches[owner]));
            for _ in 0..random(8) {
                round += 1;
                let snapshot = open(&store, &opts).await?;
                assert!(model.check(&snapshot).await?, "round {round}");
                let size = (8 + random(5)) * 4096;
                let changes: Changes = (0..1 + random(4))
                    .map(|_| {
                        let index = random(size / 4096);
                        (index, (random(8) > 0).then(|| page(round * 100 + index)))
                    })
                    .collect();
                if random(6) > 0 {
                    snapshot.publish(changes.clone(), size).await?;
                    model.apply(&changes, size);
                    continue;
                }
                let _gate = store.arm(Failpoint {
                    operation: [Operation::Put, Operation::Delete][random(2) as usize],
                    key_prefix: String::new(),
                    nth: 1 + random(4) as usize,
                    effect: [Effect::None, Effect::Visible, Effect::After][random(3) as usize],
                    cancel: false,
                });
                let result = snapshot.publish(changes.clone(), size).await;
                store.disarm();
                if random(3) == 0 {
                    store.inner.inner.crash(&[]);
                }
                if random(4) == 0 {
                    caches[owner] = fresh_namespace();
                }
                let opts = options(1, Some(&caches[owner]));
                let reopened = open(&store, &opts).await?;
                let mut new = model.clone();
                new.apply(&changes, size);
                if new.check(&reopened).await? {
                    model = new;
                } else {
                    assert!(result.is_err(), "acknowledged commit lost, round {round}");
                    assert!(model.check(&reopened).await?, "round {round}");
                }
            }
            // How the other owner's cache fares when it takes over.
            let other = &caches[1 - owner];
            let before = other.generation();
            drop(open(&store, &options(1, Some(other))).await?);
            if other.generation() == before {
                kept += 1
            } else {
                dropped += 1
            }
        }
        println!(
            "handovers: cache kept {kept}, dropped {dropped}; {} requests checked by the monitor",
            store.checked()
        );
        assert_eq!(store.violations(), Vec::<String>::new());
        assert!(kept > 50 && dropped > 50, "{kept} {dropped}");
        Ok(())
    })
}

/// Checkpoint `pages` pages so that reads reach their page keys.
async fn checkpointed(pages: u64) -> io::Result<MemoryKv> {
    let store = MemoryKv::default();
    let kv = Arc::new(store.clone());
    let changes: Changes = (0..pages).map(|i| (i, Some(page(i)))).collect();
    open(&kv, &options(4, None))
        .await?
        .publish(changes, pages * 4096)
        .await?;
    // The next save checkpoints the first; page 0 stays in the log.
    open(&kv, &options(0, None))
        .await?
        .publish(Changes::from([(0, Some(page(0)))]), pages * 4096)
        .await?;
    assert_eq!(keys(&store, PAGE_PREFIX).len() as u64, pages);
    Ok(store)
}

/// A cold open of a small database fetches every page alongside replay, so no
/// read afterwards waits for the backend, and open takes no extra round trip.
#[test]
fn a_cold_open_prefetches_a_small_database() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(checkpointed(PREFETCH_PAGES).await?));
        let without = StorageOptions {
            prefetch_pages: 0,
            ..options(4, None)
        };
        store.reset();
        open(&store, &without).await?;
        let plain = store.round_trips();

        store.reset();
        let snapshot = open(&store, &options(4, None)).await?;
        assert_eq!(
            store.round_trips(),
            plain,
            "prefetch rides along with replay"
        );
        store.reset();
        store.inner.reset_stats();
        for index in 0..PREFETCH_PAGES {
            assert_eq!(number(snapshot.block(index).await?), Some(index));
        }
        assert_eq!((store.round_trips(), store.inner.stats().gets), (0, 0));
        Ok(())
    })
}

/// A larger database is not fetched whole. A checkpointing save lists the pages
/// its session read in the working-set hint, and a cold open fetches exactly
/// those alongside replay.
#[test]
fn a_cold_open_fetches_the_working_set_hint() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(checkpointed(40).await?));
        let cold_gets = |snapshot: Snapshot, pages: Vec<u64>| {
            let store = store.clone();
            async move {
                store.inner.reset_stats();
                for index in pages {
                    assert_eq!(number(snapshot.block(index).await?), Some(index));
                }
                io::Result::Ok(store.inner.stats().gets)
            }
        };
        let snapshot = open(&store, &options(4, None)).await?;
        assert_eq!(cold_gets(snapshot, vec![5, 9, 33]).await?, 3, "no hint yet");

        // A session reads three pages, then saves with a checkpoint.
        let snapshot = open(&store, &options(0, None)).await?;
        for index in [5, 9, 33] {
            snapshot.block(index).await?;
        }
        snapshot
            .publish(Changes::from([(1, Some(page(1)))]), 40 * 4096)
            .await?;
        let hint = store.inner.get(HINT).await?.expect("a hint");
        assert_eq!(hint::decode(&hint, 32), Some(vec![5, 9, 33]));

        store.reset();
        store.inner.reset_stats();
        let snapshot = open(&store, &options(4, None)).await?;
        assert_eq!(
            store.inner.stats().gets,
            1 + 1 + 6 + 3,
            "meta, hint, probe, pages"
        );
        assert_eq!(cold_gets(snapshot, vec![5, 9, 33]).await?, 0);

        // Disabled, or with a malformed hint, opens read nothing extra.
        let off = StorageOptions {
            hint_pages: 0,
            ..options(4, None)
        };
        assert_eq!(cold_gets(open(&store, &off).await?, vec![5]).await?, 1);
        store
            .inner
            .put(HINT, Bytes::from_static(b"SH12\x09\x00garbage"))
            .await?;
        assert_eq!(
            cold_gets(open(&store, &options(4, None)).await?, vec![5]).await?,
            1
        );
        Ok(())
    })
}

/// Sequential reads of page keys fetch ahead in a growing window.
#[test]
fn sequential_reads_fetch_ahead() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(checkpointed(300).await?));
        let snapshot = open(&store, &options(4, None)).await?;
        store.reset();
        store.inner.reset_stats();
        for index in 0..300 {
            assert_eq!(number(snapshot.block(index).await?), Some(index));
        }
        let (round_trips, gets) = (store.round_trips(), store.inner.stats().gets);
        assert!(round_trips < 15, "{round_trips} round trips");
        assert_eq!(
            gets, 299,
            "page 0 is in the log; nothing else is fetched twice"
        );

        // Scattered reads do not fetch ahead.
        let snapshot = open(&store, &options(4, None)).await?;
        store.inner.reset_stats();
        for index in [7, 100, 3, 250, 42] {
            assert_eq!(number(snapshot.block(index).await?), Some(index));
        }
        assert_eq!(store.inner.stats().gets, 5);
        Ok(())
    })
}

/// With a cache, open probes exactly to where this owner left the log, and
/// every save is one round trip, including its checkpoint slice.
#[test]
fn an_owner_probes_exactly_and_saves_in_one_round_trip() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(MemoryKv::default()));
        let cache = fresh_namespace();
        let opts = options(16, Some(&cache));
        for round in 0..40u64 {
            store.reset();
            store.inner.reset_stats();
            let snapshot = open(&store, &opts).await?;
            if round > 0 {
                assert_eq!(store.round_trips(), 1, "open {round}");
                // The log, plus the key after it, plus meta.
                let expected = snapshot.log.transactions.len() as u64 + 2;
                assert_eq!(store.inner.stats().gets, expected, "open {round}");
            }
            // Transactions of 20 pages: a checkpoint slice spans more than 16 puts.
            let changes: Changes = (0..20).map(|i| (i, Some(page(round * 100 + i)))).collect();
            store.reset();
            snapshot.publish(changes, 20 * 4096).await?;
            assert_eq!(store.round_trips(), 1, "save {round}");
        }
        Ok(())
    })
}

/// A session that continues after each commit reads exactly what a reopen
/// would, as checkpoints drop its transactions from the log, including pages
/// fetched ahead at open that a commit rewrote without reading.
#[test]
fn a_continued_snapshot_matches_a_reopen() -> io::Result<()> {
    futures::executor::block_on(async {
        for cached in [false, true] {
            let cache = cached.then(fresh_namespace);
            let store = Arc::new(checkpointed(PREFETCH_PAGES).await?);
            let opts = options(1, cache.as_ref());
            let mut model = Model::default();
            let all: Changes = (0..PREFETCH_PAGES).map(|i| (i, Some(page(i)))).collect();
            model.apply(&all, PREFETCH_PAGES * 4096);

            // Page 5 is fetched ahead at open and rewritten unread.
            let mut snapshot = open(&store, &opts).await?;
            let mut rewrite = Changes::from([(5, Some(page(500)))]);
            for round in 0..12u64 {
                let size = PREFETCH_PAGES * 4096;
                let (_, next) = snapshot.commit(rewrite.clone(), size, false).await?;
                model.apply(&rewrite, size);
                snapshot = next;
                rewrite = Changes::from([(round % 3, Some(page(1000 + round)))]);
            }
            assert!(
                !snapshot.log.images.contains_key(&5),
                "the rewrite was checkpointed out of the log"
            );
            assert!(model.check(&snapshot).await?, "cached={cached}");
            assert!(model.check(&open(&store, &opts).await?).await?);
            assert_collected(&store).await?;
        }
        Ok(())
    })
}

/// With `checkpoint_on_save`, commits only append, and compacting writes each
/// changed page once, empties the log and leaves a `meta` past it.
#[test]
fn compacting_writes_each_page_once_and_empties_the_log() -> io::Result<()> {
    futures::executor::block_on(async {
        for cached in [false, true] {
            let cache = cached.then(fresh_namespace);
            let store = Arc::new(MemoryKv::default());
            let opts = StorageOptions {
                checkpoint_on_save: true,
                ..options(64, cache.as_ref())
            };
            let mut model = Model::default();
            let mut snapshot = open(&store, &opts).await?;
            for round in 0..12 {
                let (changes, size) = dirty(round);
                let (report, after) = snapshot.commit(changes.clone(), size, false).await?;
                assert_eq!(report.checkpointed_pages, 0);
                model.apply(&changes, size);
                snapshot = after;
            }
            assert!(keys(&store, PAGE_PREFIX).is_empty(), "commits only append");
            assert_eq!(keys(&store, LOG_PREFIX).len(), 12);

            store.reset_stats();
            let (report, after) = snapshot.compact().await?;
            assert!(report.cleanup_error.is_none());
            let distinct = model.pages.len();
            assert_eq!(report.checkpointed_pages, distinct);
            // Each page once, `meta` twice, and the hint if it changed.
            let hinted = store.stats().write_keys.contains(HINT) as u64;
            assert_eq!(store.stats().puts, distinct as u64 + 2 + hinted);
            assert_eq!(keys(&store, PAGE_PREFIX).len(), distinct);
            assert!(keys(&store, LOG_PREFIX).is_empty());
            let meta = Meta::decode(&store.get(META).await?.unwrap())?;
            assert_eq!((meta.log_start, meta.deleted_below), (13, 13));
            assert!(model.check(&after).await?);
            let cold = StorageOptions {
                cache: None,
                ..opts.clone()
            };
            assert!(model.check(&open(&store, &cold).await?).await?);

            // Nothing left to do.
            store.reset_stats();
            let (report, after) = after.compact().await?;
            assert_eq!((report.checkpointed_pages, store.stats().puts), (0, 0u64));
            // The next commit appends to the empty log.
            let (changes, size) = dirty(12);
            after.publish(changes.clone(), size).await?;
            model.apply(&changes, size);
            assert_eq!(keys(&store, LOG_PREFIX), [log_key(13)]);
            assert!(model.check(&open(&store, &cold).await?).await?);
        }
        Ok(())
    })
}

/// Fail every mutation of a compaction in every outcome, with and without a
/// crash: the database reads the same, a later commit works, and nothing
/// below the collected log survives.
#[test]
fn every_compact_mutation_is_crash_safe() -> io::Result<()> {
    futures::executor::block_on(async {
        let mut cases = 0;
        for cached in [false, true] {
            for operation in [Operation::Put, Operation::Delete] {
                'ordinal: for nth in 1.. {
                    for effect in [Effect::None, Effect::Visible, Effect::After] {
                        for crash in [false, true] {
                            let cache = cached.then(fresh_namespace);
                            let store = Arc::new(MonitorKv::seeded(
                                FaultKv::new(MemoryKv::default(), 0),
                                Vec::new(),
                            ));
                            let mut opts = StorageOptions {
                                page_compression: PageCompression::Raw,
                                checkpoint_on_save: true,
                                ..options(64, cache.as_ref())
                            };
                            let mut model = Model::default();
                            let mut snapshot = open(&store, &opts).await?;
                            for round in 0..6 {
                                let (changes, size) = dirty(round);
                                snapshot = snapshot.commit(changes.clone(), size, false).await?.1;
                                model.apply(&changes, size);
                            }
                            let _gate = store.arm(Failpoint {
                                operation,
                                key_prefix: String::new(),
                                nth,
                                effect,
                                cancel: false,
                            });
                            let result = snapshot.compact().await;
                            let faulted = store.events().iter().any(|e| e.faulted);
                            store.disarm();
                            if !faulted {
                                assert!(result?.0.cleanup_error.is_none());
                                break 'ordinal;
                            }
                            cases += 1;
                            if crash {
                                store.inner.inner.crash(&[]);
                                opts.cache = cached.then(fresh_namespace);
                            }
                            let reopened = open(&store, &opts).await?;
                            assert!(model.check(&reopened).await?, "{operation:?} {nth} {effect:?} {crash}");
                            let (changes, size) = dirty(51);
                            reopened.publish(changes.clone(), size).await?;
                            model.apply(&changes, size);
                            // The follow-up commit survives a crash, and a
                            // compaction after it completes.
                            store.inner.inner.crash(&[]);
                            opts.cache = cached.then(fresh_namespace);
                            let (report, after) = open(&store, &opts).await?.compact().await?;
                            assert!(report.cleanup_error.is_none());
                            assert!(model.check(&after).await?);
                            assert!(model.check(&open(&store, &opts).await?).await?);
                            assert_collected(&store.inner.inner).await?;
                            assert_eq!(store.violations(), Vec::<String>::new());
                        }
                    }
                }
            }
        }
        assert!(cases > 40, "{cases}");
        println!("interrupted compactions: {cases} cases");
        Ok(())
    })
}
