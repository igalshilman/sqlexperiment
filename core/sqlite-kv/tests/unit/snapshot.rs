use super::*;
use crate::{
    PageCacheConfig,
    format::{COMMIT, HINT, LOG_PREFIX, PAGE_PREFIX},
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
/// The durable commit record.
async fn record(store: &MemoryKv) -> io::Result<Commit> {
    match store.get(COMMIT).await? {
        Some(bytes) => Commit::decode(&bytes),
        None => Ok(Commit::default()),
    }
}
/// Every stored value is named by the record, listed as its garbage, or
/// debris at a number it has not allocated, which a later save overwrites.
async fn assert_collected(store: &MemoryKv) -> io::Result<()> {
    let commit = record(store).await?;
    let named = commit.stored_values();
    for key in keys(store, LOG_PREFIX) {
        let n = u64::from_str_radix(key.trim_start_matches(LOG_PREFIX), 16).unwrap();
        assert!(
            named.contains(&n) || commit.garbage.contains(&n) || n >= commit.next_segment,
            "{key}: unnamed, not garbage, below {}",
            commit.next_segment
        );
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
        assert_eq!(store.keys().into_iter().collect::<Vec<_>>(), [COMMIT]);

        // Checkpointing the whole log moves pages to their keys (a hole is a
        // delete) and empties the record's log, in a new epoch.
        let all = StorageOptions {
            prefetch_pages: 0,
            ..options(0, None)
        };
        let snapshot = open(&store, &all).await?;
        let report = snapshot
            .publish(Changes::from([(3, None)]), 4 * 4096)
            .await?;
        assert_eq!(report.checkpointed_pages, 4);
        assert_eq!(keys(&store, PAGE_PREFIX).len(), 3);
        assert!(keys(&store, LOG_PREFIX).is_empty());
        let commit = record(&store).await?;
        assert_eq!(
            (commit.version, commit.epoch, commit.size),
            (2, 1, 4 * 4096)
        );
        assert!(commit.log.is_empty());

        let snapshot = open(&store, &all).await?;
        store.reset_stats();
        assert_eq!(number(snapshot.block(1).await?), Some(1));
        assert_eq!(store.stats().gets, 1, "no map: one get per page");
        assert_eq!(number(snapshot.block(3).await?), None);
        assert_eq!(snapshot.size(), 4 * 4096);
        Ok(())
    })
}

/// Without a cache, every save is one round trip, the ones that checkpoint
/// too: the record, the pages and the record again share one sync.
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
            round_trips.push((store.round_trips(), report.checkpointed_pages > 0));
            model.apply(&changes, size);
            // Small transactions are inline: no stored segment.
            assert!(keys(&store.inner, LOG_PREFIX).is_empty());
        }
        assert!(
            round_trips.iter().all(|&(rt, _)| rt == 1),
            "{round_trips:?}"
        );
        let checkpoints = round_trips
            .iter()
            .filter(|&&(_, checkpointed)| checkpointed)
            .count();
        assert!((3..=6).contains(&checkpoints), "{round_trips:?}");
        assert!(model.check(&open(&store, &opts).await?).await?);
        Ok(())
    })
}

/// With an owner cache, every steady-state open and save is one round trip,
/// and a restarted owner still recovers.
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
                assert_eq!(store.inner.stats().gets, 1, "the record alone");
            }
            assert!(model.check(&snapshot).await?, "round {round}");
            let (changes, size) = dirty(round);
            store.reset();
            snapshot.publish(changes.clone(), size).await?;
            assert_eq!(store.round_trips(), 1, "save {round}");
            model.apply(&changes, size);
            assert_collected(&store.inner).await?;
        }
        // A restarted owner starts from the durable record.
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

        // Another writer checkpoints: the epoch changes and the cache drops
        // its pages.
        for round in 10..13 {
            let (changes, size) = dirty(round);
            open(&store, &other)
                .await?
                .publish(changes.clone(), size)
                .await?;
            model.apply(&changes, size);
        }
        assert!(model.check(&open(&store, &owner).await?).await?);

        // It rewrites pages in place, but a crash loses the checkpoint's
        // record: the cache keeps stale pages, and the log still overrides
        // every one.
        let snapshot = open(&store, &owner).await?;
        assert!(model.check(&snapshot).await?);
        for round in 20..22 {
            let (changes, size) = dirty(round);
            let snapshot = open(&store, &other).await?;
            let _gate = store.arm(Failpoint {
                operation: Operation::Sync,
                key_prefix: String::new(),
                nth: 1,
                effect: Effect::None,
                cancel: false,
            });
            assert!(snapshot.publish(changes.clone(), size).await.is_err());
            store.disarm();
            // Every write but the last, the checkpoint's record.
            let writes = store.inner.unsynced();
            assert_eq!(writes.last().map(|(key, _)| key.as_str()), Some(COMMIT));
            store.inner.crash_keeping(writes.len() - 1);
            model.apply(&changes, size);
        }
        assert!(model.check(&open(&store, &owner).await?).await?);
        Ok(())
    })
}

#[test]
fn large_transactions_are_stored_and_failed_ones_leave_only_debris() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(MonitorKv::new(FaultKv::new(
            MemoryKv::default(),
            3,
        ))));
        let memory = || store.inner.inner.inner.clone();
        let large = StorageOptions {
            inline_log_bytes: 0,
            ..options(4, None)
        };
        let big = |n: u64| -> Changes { (0..300).map(|i| (i, Some(page(i + n)))).collect() };
        let snapshot = open(&store, &large).await?;
        store.reset();
        snapshot.publish(big(0), 300 * 4096).await?;
        assert_eq!(store.round_trips(), 1, "the segments, then the record");
        assert_eq!(keys(&memory(), LOG_PREFIX), [log_key(0), log_key(1)]);
        assert_eq!(record(&memory()).await?.next_segment, 2);

        // The next one's record fails, and a crash keeps its segments: they
        // are debris.
        let snapshot = open(&store, &large).await?;
        let _gate = store.inner.arm(Failpoint {
            operation: Operation::Put,
            key_prefix: COMMIT.into(),
            nth: 1,
            effect: Effect::None,
            cancel: false,
        });
        assert!(snapshot.publish(big(1), 300 * 4096).await.is_err());
        store.inner.disarm();
        memory().crash_keeping(2);
        assert_eq!(keys(&memory(), LOG_PREFIX).len(), 4);
        let snapshot = open(&store, &large).await?;
        assert_eq!(number(snapshot.block(299).await?), Some(299));
        // The next large save overwrites the debris.
        snapshot.publish(big(7), 300 * 4096).await?;
        assert_eq!(keys(&memory(), LOG_PREFIX).len(), 4);
        let snapshot = open(&store, &large).await?;
        assert_eq!(number(snapshot.block(5).await?), Some(12));
        assert_eq!(store.inner.violations(), Vec::<String>::new());
        Ok(())
    })
}

/// Build a log after `saves` saves with `checkpoint_after = 1`: after an odd
/// number the next save also checkpoints; after an even one it only commits.
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

/// Crash a save, keeping each prefix of its writes, with the owner cache
/// kept or lost: the save is atomic, later saves work, and every stored value
/// is named, garbage or above the allocated numbers.
#[test]
fn a_save_is_crash_safe_at_every_write() -> io::Result<()> {
    futures::executor::block_on(async {
        let (mut cases, mut checked) = (0, 0);
        // (cache, prepared saves, large save: stored segments). After an odd
        // number of saves the next checkpoints.
        let scenarios = [
            (false, 3, false),
            (false, 4, false),
            (true, 3, false),
            (true, 4, false),
            (false, 3, true),
            (true, 4, true),
        ];
        for (cached, saves, large) in scenarios {
            let cache = cached.then(fresh_namespace);
            let (base, old) = prepared(cache.as_ref(), saves).await?;
            let store = Arc::new(FaultKv::new(base.fork(), 0));
            let opts = StorageOptions {
                page_compression: PageCompression::Raw,
                inline_log_bytes: if large { 0 } else { usize::MAX },
                ..options(1, cache.as_ref())
            };
            let snapshot = open(&store, &opts).await?;
            let _gate = store.arm(Failpoint {
                operation: Operation::Sync,
                key_prefix: String::new(),
                nth: 1,
                effect: Effect::None,
                cancel: false,
            });
            let (changes, size) = if large {
                let pages = (0..70).map(|i| (i, Some(page(500 + i))));
                (pages.collect(), 70 * 4096)
            } else {
                dirty(50)
            };
            assert!(snapshot.publish(changes.clone(), size).await.is_err());
            store.disarm();
            let mut new = old.clone();
            new.apply(&changes, size);
            let writes = store.inner.unsynced().len();
            for kept in 0..=writes {
                for restarted in [false, true] {
                    cases += 1;
                    let memory = store.inner.fork();
                    memory.crash_keeping(kept);
                    let store = Arc::new(MonitorKv::seeded(memory.clone(), memory.values()));
                    let mut opts = opts.clone();
                    // The owner continues with its cache only to read: the
                    // cache follows one history, and the forks are each
                    // another.
                    opts.cache = match restarted {
                        true => cached.then(fresh_namespace),
                        false => cache.clone(),
                    };
                    let reopened = open(&store, &opts).await?;
                    let committed = new.check(&reopened).await?;
                    assert!(
                        committed || old.check(&reopened).await?,
                        "{kept} of {writes}"
                    );
                    assert!(committed || kept < writes, "a stored commit was lost");
                    let mut model = if committed { new.clone() } else { old.clone() };
                    drop(reopened);
                    opts.cache = cached.then(fresh_namespace);
                    let (changes, size) = dirty(51);
                    open(&store, &opts)
                        .await?
                        .publish(changes.clone(), size)
                        .await?;
                    model.apply(&changes, size);
                    // The follow-up commit survives a crash.
                    memory.crash();
                    opts.cache = cached.then(fresh_namespace);
                    assert!(model.check(&open(&store, &opts).await?).await?);
                    assert_collected(&memory).await?;
                    assert_eq!(store.violations(), Vec::<String>::new());
                    checked += store.checked();
                }
            }
        }
        assert!(cases > 40, "{cases}");
        println!("interrupted saves: {cases} cases, {checked} requests checked by the monitor");
        Ok(())
    })
}

/// Two owner processes, each with its own cache, hand the namespace back and
/// forth after random tenures. Saves fail in random places and outcomes, the
/// backend sometimes loses a suffix of the unsynced writes, and owners
/// sometimes restart.
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
                let operation = [Operation::Put, Operation::Delete, Operation::Sync];
                let _gate = store.arm(Failpoint {
                    operation: operation[random(3) as usize],
                    key_prefix: String::new(),
                    nth: 1 + random(4) as usize,
                    effect: [Effect::None, Effect::After][random(2) as usize],
                    cancel: false,
                });
                let result = snapshot.publish(changes.clone(), size).await;
                store.disarm();
                if random(3) == 0 {
                    // A crash keeps a prefix of the writes since the last sync.
                    let writes = store.inner.inner.unsynced().len() as u64;
                    store.inner.inner.crash_keeping(random(writes + 1) as usize);
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
    // The next save checkpoints the whole log.
    open(&kv, &options(0, None))
        .await?
        .publish(Changes::from([(0, Some(page(0)))]), pages * 4096)
        .await?;
    assert_eq!(keys(&store, PAGE_PREFIX).len() as u64, pages);
    Ok(store)
}

/// A cold open of a small database fetches every page once it has read the
/// record (a round trip more than without), so no read afterwards waits for
/// the backend.
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
            (plain, store.round_trips()),
            (1, 2),
            "the record, then the pages"
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
            1 + 1 + 3,
            "the record, the hint, pages"
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
            .put(HINT, Bytes::from_static(b"SH14\x09\x00garbage"))?;
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
        assert_eq!(gets, 300, "nothing is fetched twice");

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

/// With a cache, open reads the record alone, and every save is one round
/// trip, the ones that checkpoint too (with a log the record holds inline).
#[test]
fn an_owner_opens_and_saves_in_one_round_trip() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(MemoryKv::default()));
        let cache = fresh_namespace();
        let opts = StorageOptions {
            inline_log_bytes: usize::MAX,
            ..options(16, Some(&cache))
        };
        for round in 0..40u64 {
            store.reset();
            store.inner.reset_stats();
            let snapshot = open(&store, &opts).await?;
            if round > 0 {
                assert_eq!(store.round_trips(), 1, "open {round}");
                assert_eq!(store.inner.stats().gets, 1, "open {round}");
            }
            // Transactions of 20 pages: a checkpoint spans more than 16 puts.
            let changes: Changes = (0..20).map(|i| (i, Some(page(round * 100 + i)))).collect();
            store.reset();
            snapshot.publish(changes, 20 * 4096).await?;
            assert_eq!(store.round_trips(), 1, "save {round}");
        }
        Ok(())
    })
}

/// A commit that would take the inline log past its budget seals it into a
/// stored value, put before its record, which names it. Every commit is one
/// round trip, an owner whose cache holds the values opens on the record
/// alone, and one without reads them too.
#[test]
fn a_commit_seals_the_inline_log_into_a_stored_value() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(LatencyKv::new(MemoryKv::default()));
        let cache = fresh_namespace();
        let opts = StorageOptions {
            inline_log_bytes: 6 * 1024,
            ..options(1000, Some(&cache))
        };
        // Pages of noise: about 2 KiB of log each.
        let noisy = |seed: u64| -> Page {
            let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
            let mut p: Vec<u8> = (0..BLOCK_SIZE)
                .map(|i| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    if i % 2 == 0 { state as u8 } else { 0 }
                })
                .collect();
            p[..8].copy_from_slice(&seed.to_le_bytes());
            p.into()
        };
        let mut sealed = 0;
        for round in 0..12u64 {
            let snapshot = open(&store, &opts).await?;
            let changes: Changes = (0..2).map(|i| (i, Some(noisy(round * 10 + i)))).collect();
            store.reset();
            snapshot.publish(changes, 2 * 4096).await?;
            assert_eq!(store.round_trips(), 1, "commit {round}");
            sealed = record(&store.inner).await?.stored_values().len();
        }
        assert!(sealed >= 2, "{sealed} sealed values");
        store.reset();
        store.inner.reset_stats();
        let snapshot = open(&store, &opts).await?;
        assert_eq!(
            store.inner.stats().gets,
            1,
            "the cache holds the sealed values"
        );
        let cold = open(&store, &options(1000, None)).await?;
        for (index, snapshot) in [&snapshot, &cold].into_iter().enumerate() {
            for i in 0..2 {
                assert_eq!(snapshot.block(i).await?, Some(noisy(110 + i)), "{index}");
            }
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
                let (_, next) = snapshot.commit(rewrite.clone(), size, None, None).await?;
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

/// Below the thresholds commits only append to the record, and the commit that
/// checkpoints writes each changed page once and empties the record's log.
#[test]
fn a_checkpoint_writes_each_page_once_and_empties_the_log() -> io::Result<()> {
    futures::executor::block_on(async {
        for cached in [false, true] {
            let cache = cached.then(fresh_namespace);
            let store = Arc::new(MemoryKv::default());
            let opts = StorageOptions {
                checkpoint_log_bytes: usize::MAX,
                ..options(64, cache.as_ref())
            };
            let mut model = Model::default();
            let mut snapshot = open(&store, &opts).await?;
            for round in 0..12 {
                let (changes, size) = dirty(round);
                let (report, after) = snapshot.commit(changes.clone(), size, None, None).await?;
                assert_eq!(report.checkpointed_pages, 0);
                model.apply(&changes, size);
                snapshot = after;
            }
            assert!(keys(&store, PAGE_PREFIX).is_empty(), "commits only append");
            assert_eq!(record(&store).await?.log.len(), 12);

            // The next commit checkpoints.
            let checkpointing = StorageOptions {
                checkpoint_after: 0,
                ..opts.clone()
            };
            drop(snapshot);
            let snapshot = open(&store, &checkpointing).await?;
            store.reset_stats();
            let (changes, size) = dirty(12);
            let (report, after) = snapshot.commit(changes.clone(), size, None, None).await?;
            model.apply(&changes, size);
            let distinct = model.pages.len();
            assert_eq!(report.checkpointed_pages, distinct);
            // The record, each page once, the hint if it changed, and the
            // record again, in one sync.
            let hinted = store.stats().write_keys.contains(HINT) as u64;
            assert_eq!(store.stats().puts, 2 + distinct as u64 + hinted);
            assert_eq!(store.stats().syncs, 1);
            assert_eq!(keys(&store, PAGE_PREFIX).len(), distinct);
            let commit = record(&store).await?;
            assert_eq!((commit.version, commit.epoch), (13, 1));
            assert!(commit.log.is_empty());
            assert!(model.check(&after).await?);
            let cold = StorageOptions {
                cache: None,
                ..opts.clone()
            };
            assert!(model.check(&open(&store, &cold).await?).await?);

            // The next commit appends to the empty log.
            drop(after);
            let (changes, size) = dirty(13);
            open(&store, &opts)
                .await?
                .publish(changes.clone(), size)
                .await?;
            model.apply(&changes, size);
            assert_eq!(record(&store).await?.log.len(), 1);
            assert!(model.check(&open(&store, &cold).await?).await?);
        }
        Ok(())
    })
}
