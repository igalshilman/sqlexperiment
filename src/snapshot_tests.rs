async fn assert_no_orphans(store: Arc<MemoryKv>) -> io::Result<()> {
    let snapshot = Snapshot::open(store.clone()).await?;
    let mut expected = snapshot.reachable().await?;
    if snapshot.original_head.is_some() {
        expected.insert(HEAD.to_owned());
    }
    assert_eq!(store.keys(), expected, "untracked objects remain");
    Ok(())
}

use super::*;
use crate::kv::{MemoryKv, yield_once};
use std::sync::atomic::{AtomicUsize, Ordering};

fn options(layout: MapLayout) -> StorageOptions {
    StorageOptions {
        layout,
        write_concurrency: 8,
    }
}
fn page(n: u64) -> Vec<u8> {
    let mut p = vec![0; BLOCK_SIZE];
    p[..8].copy_from_slice(&n.to_le_bytes());
    p
}

#[test]
fn one_page_cache_hits_evicts_and_does_not_cross_snapshots() -> io::Result<()> {
    futures::executor::block_on(async {
        for layout in [
            MapLayout::Radix,
            MapLayout::InlineRoot,
            MapLayout::HeadOverrides,
        ] {
            let store = Arc::new(MemoryKv::default());
            Snapshot::open_with_options(store.clone(), options(layout))
                .await?
                .publish(
                    Changes::from([(0, Some(page(1))), (1, Some(page(2)))]),
                    8192,
                )
                .await?;
            let snapshot = Snapshot::open(store.clone()).await?;
            store.reset_stats();
            for _ in 0..5 {
                assert_eq!(snapshot.block(0).await?, Some(page(1)));
            }
            assert_eq!(store.stats().by_kind["block"].gets, 1);
            assert_eq!(snapshot.block(1).await?, Some(page(2)));
            assert_eq!(snapshot.block(0).await?, Some(page(1)));
            assert_eq!(store.stats().by_kind["block"].gets, 3);
            let fresh = Snapshot::open(store.clone()).await?;
            assert_eq!(fresh.block(0).await?, Some(page(1)));
            assert_eq!(store.stats().by_kind["block"].gets, 4);
        }
        Ok(())
    })
}

#[test]
fn missing_and_malformed_pages_are_not_cached() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        Snapshot::open(store.clone())
            .await?
            .publish(Changes::from([(0, Some(page(1)))]), 4096)
            .await?;
        let snapshot = Snapshot::open(store.clone()).await?;
        let block_key = key("block", &snapshot.block_id(0).await?.unwrap());
        // Deliberately violate immutability to simulate a repaired failed read.
        store.delete(&block_key).await?;
        assert!(snapshot.block(0).await.is_err());
        store.put(&block_key, vec![0; BLOCK_SIZE - 1]).await?;
        assert!(snapshot.block(0).await.is_err());
        store.put(&block_key, page(1)).await?;
        assert_eq!(snapshot.block(0).await?, Some(page(1)));
        store.reset_stats();
        assert_eq!(snapshot.block(0).await?, Some(page(1)));
        assert_eq!(store.stats().gets, 0);
        Ok(())
    })
}

#[test]
fn v7_head_or_pending_record_is_rejected_without_mutation() -> io::Result<()> {
    futures::executor::block_on(async {
        for old_key in ["v7/head", "v7/pending_save"] {
            let store = Arc::new(MemoryKv::default());
            store.put(old_key, vec![1]).await?;
            store.reset_stats();
            assert!(Snapshot::open(store.clone()).await.is_err());
            assert_eq!(store.stats().puts + store.stats().deletes, 0);
        }
        Ok(())
    })
}

#[test]
fn repeated_sparse_rewrites_leave_exactly_the_current_structural_objects() -> io::Result<()> {
    futures::executor::block_on(async {
        for layout in [
            MapLayout::Radix,
            MapLayout::InlineRoot,
            MapLayout::HeadOverrides,
        ] {
            let store = Arc::new(MemoryKv::default());
            let mut expected = BTreeMap::new();
            let mut rng = 17u64;
            let addresses = [0, 1, 255, 256, 257, 65535, 65536, 65537, 1 << 24, 1 << 40];
            for round in 0..150 {
                let snapshot = Snapshot::open_with_options(store.clone(), options(layout)).await?;
                let mut dirty = Changes::new();
                for _ in 0..7 {
                    rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let index = addresses[((rng >> 32) % addresses.len() as u64) as usize];
                    let value = (rng & 3 != 0).then(|| page(round));
                    if let Some(value) = &value {
                        expected.insert(index, value.clone());
                    } else {
                        expected.remove(&index);
                    }
                    dirty.insert(index, value);
                }
                let size = expected.last_key_value().map_or(0, |(i, _)| (i + 1) * 4096);
                assert!(snapshot.publish(dirty, size).await?.cleanup_error.is_none());
                store.crash(&[]);
                let current = Snapshot::open(store.clone()).await?;
                for index in addresses {
                    assert_eq!(current.block(index).await?, expected.get(&index).cloned());
                }
                assert_no_orphans(store.clone()).await?;
            }
            let current = Snapshot::open(store.clone()).await?;
            current
                .publish(expected.keys().map(|i| (*i, None)).collect(), 0)
                .await?;
            assert_no_orphans(store.clone()).await?;
            assert_eq!(store.keys(), BTreeSet::from([HEAD.to_owned()]));
        }
        Ok(())
    })
}

#[test]
fn compare_map_costs_on_dense_16mib_database() -> io::Result<()> {
    futures::executor::block_on(async {
        for layout in [
            MapLayout::Radix,
            MapLayout::InlineRoot,
            MapLayout::HeadOverrides,
        ] {
            let base = Arc::new(MemoryKv::default());
            let snapshot = Snapshot::open_with_options(base.clone(), options(layout)).await?;
            snapshot
                .publish(
                    (0..4096).map(|i| (i, Some(page(i)))).collect(),
                    16 * 1024 * 1024,
                )
                .await?;
            for (name, indexes, expected_puts) in [
                ("one", vec![42], [5, 4, 3]),
                ("ten_clustered", (0..10).collect(), [14, 13, 12]),
                (
                    "ten_scattered",
                    (0..10).map(|i| i * 256).collect(),
                    [23, 22, 12],
                ),
                ("65_fold", (0..65).collect(), [69, 68, 69]),
            ] {
                let store = Arc::new(base.fork());
                let snapshot = Snapshot::open(store.clone()).await?;
                store.reset_stats();
                snapshot
                    .publish(
                        indexes
                            .iter()
                            .map(|i| (*i, Some(page(i + 10000))))
                            .collect(),
                        snapshot.size,
                    )
                    .await?;
                let stats = store.stats();
                assert_eq!(stats.puts, expected_puts[layout as usize]);
                assert_eq!(
                    stats
                        .write_keys
                        .iter()
                        .filter(|k| k.contains("/block/"))
                        .count(),
                    indexes.len()
                );
                let head_bytes = store.get(HEAD).await?.unwrap().len();
                store.crash(&[]);
                store.reset_stats();
                let reopened = Snapshot::open(store.clone()).await?;
                assert_eq!(
                    reopened.block(indexes[0]).await?,
                    Some(page(indexes[0] + 10000))
                );
                let cold = store.stats();
                println!(
                    "MAP {layout:?} {name}: puts={} bytes={} metadata={} head={head_bytes} cold_gets={} cold_bytes={}",
                    stats.puts,
                    stats.bytes_written,
                    stats.bytes_written - indexes.len() as u64 * BLOCK_SIZE as u64,
                    cold.gets,
                    cold.bytes_read
                );
                for i in indexes {
                    assert_eq!(reopened.block(i).await?, Some(page(i + 10000)));
                }
                assert_eq!(reopened.block(4095).await?, Some(page(4095)));
            }
        }
        Ok(())
    })
}

#[test]
fn layouts_preserve_sparse_paths_fold_tombstones_without_gc() -> io::Result<()> {
    futures::executor::block_on(async {
        for layout in [
            MapLayout::Radix,
            MapLayout::InlineRoot,
            MapLayout::HeadOverrides,
        ] {
            let store = Arc::new(MemoryKv::default());
            let mut snapshot = Snapshot::open_with_options(store.clone(), options(layout)).await?;
            let mut expected: BTreeMap<u64, Vec<u8>> = (0..70).map(|i| (i, page(i))).collect();
            snapshot
                .publish(
                    expected
                        .iter()
                        .map(|(i, p)| (*i, Some(p.clone())))
                        .collect(),
                    70 * 4096,
                )
                .await?;
            snapshot = Snapshot::open(store.clone()).await?;
            let mut changes = Changes::new();
            // 64 head entries exactly: deletion, replacement and sparse growth.
            for i in 0..62 {
                changes.insert(i, Some(page(i + 100)));
                expected.insert(i, page(i + 100));
            }
            changes.insert(63, None);
            expected.remove(&63);
            changes.insert(1 << 40, Some(page(900)));
            expected.insert(1 << 40, page(900));
            let large = ((1 << 40) + 1) * 4096;
            snapshot.publish(changes, large).await?;
            snapshot = Snapshot::open(store.clone()).await?;
            if layout == MapLayout::HeadOverrides {
                assert_eq!(snapshot.overrides.len(), 64);
            }
            drop(snapshot);
            assert_no_orphans(store.clone()).await?;
            snapshot = Snapshot::open(store.clone()).await?;
            // New logical address is the 65th override and forces a fold.
            snapshot
                .publish(Changes::from([(256, Some(page(901)))]), large)
                .await?;
            expected.insert(256, page(901));
            store.crash(&[]);
            snapshot = Snapshot::open(store.clone()).await?;
            assert!(snapshot.overrides.is_empty());
            for (i, p) in &expected {
                assert_eq!(snapshot.block(*i).await?, Some(p.clone()));
            }
            assert!(snapshot.block(63).await?.is_none());
            let counter = snapshot.last_id;
            // Empty snapshot still carries counter. Save removes every object.
            snapshot
                .publish(expected.keys().map(|i| (*i, None)).collect(), 0)
                .await?;
            let empty = Snapshot::open(store.clone()).await?;
            assert!(empty.last_id >= counter);
            let old_counter = empty.last_id;
            drop(empty);
            assert_no_orphans(store.clone()).await?;
            assert_eq!(store.block_count(), 0);
            Snapshot::open(store.clone())
                .await?
                .publish(Changes::from([(0, Some(page(902)))]), 4096)
                .await?;
            store.crash(&[]);
            let restored = Snapshot::open(store).await?;
            assert!(restored.last_id > old_counter);
            assert_eq!(restored.block(0).await?, Some(page(902)));
        }
        Ok(())
    })
}

#[test]
fn orphan_ids_are_reused_and_exhaustion_precedes_all_mutations() -> io::Result<()> {
    futures::executor::block_on(async {
        for layout in [
            MapLayout::Radix,
            MapLayout::InlineRoot,
            MapLayout::HeadOverrides,
        ] {
            let store = Arc::new(MemoryKv::default());
            let snapshot = Snapshot::open_with_options(store.clone(), options(layout)).await?;
            snapshot
                .publish(Changes::from([(0, Some(page(1)))]), 4096)
                .await?;
            let committed = store.get(HEAD).await?.unwrap();
            let snapshot = Snapshot::open(store.clone()).await?;
            let high = snapshot.last_id;
            // Interrupted attempt: intent is durable before either new object.
            let mut attempted_head = committed.clone();
            attempted_head[12..20].copy_from_slice(&(high + 2).to_le_bytes());
            let intent = PendingSave {
                old_head: Some(committed.clone()),
                new_head: attempted_head,
                created: Objects::from([("block", high + 1), ("block", high + 2)]),
                retired: Objects::new(),
            };
            store.put(PENDING, intent.encode()?).await?;
            store.put(&key("block", &(high + 1)), page(999)).await?;
            store.put(&key("block", &(high + 2)), page(998)).await?;
            store.crash(&[]);
            assert_eq!(store.get(HEAD).await?, Some(committed));
            let snapshot = Snapshot::open(store.clone()).await?;
            snapshot
                .publish(Changes::from([(0, Some(page(2)))]), 4096)
                .await?;
            assert_eq!(store.get(&key("block", &(high + 1))).await?, Some(page(2)));
            store.crash(&[]);
            assert_eq!(
                Snapshot::open(store.clone()).await?.block(0).await?,
                Some(page(2))
            );
            let mut head = store.get(HEAD).await?.unwrap();
            head[12..20].copy_from_slice(&u64::MAX.to_le_bytes());
            store.put(HEAD, head).await?;
            store.reset_stats();
            assert!(
                Snapshot::open(store.clone())
                    .await?
                    .publish(Changes::from([(0, Some(page(3)))]), 4096)
                    .await
                    .is_err()
            );
            assert_eq!(store.stats().puts, 0);
        }
        Ok(())
    })
}

struct ReorderingKv {
    inner: MemoryKv,
    active: AtomicUsize,
    maximum: AtomicUsize,
    finished: Mutex<Vec<u64>>,
    fail_id: Option<u64>,
}
struct Active<'a>(&'a AtomicUsize);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl KvStore for ReorderingKv {
    fn get<'a>(&'a self, k: &'a str) -> BoxFuture<'a, io::Result<Option<Vec<u8>>>> {
        self.inner.get(k)
    }
    fn delete<'a>(&'a self, k: &'a str) -> BoxFuture<'a, io::Result<()>> {
        self.inner.delete(k)
    }
    fn put<'a>(&'a self, k: &'a str, v: Vec<u8>) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            if k == HEAD || k == PENDING {
                assert_eq!(self.active.load(Ordering::SeqCst), 0);
                return self.inner.put(k, v).await;
            }
            let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            let _active = Active(&self.active);
            self.maximum.fetch_max(n, Ordering::SeqCst);
            let id = u64::from_str_radix(k.rsplit('/').next().unwrap(), 16).unwrap();
            for _ in 0..(1 + (17 - id % 17)) {
                yield_once().await;
            }
            if self.fail_id == Some(id) {
                return Err(io::Error::other("injected object failure"));
            }
            self.inner.put(k, v).await?;
            self.finished.lock().unwrap().push(id);
            Ok(())
        })
    }
}
#[test]
fn concurrent_puts_reorder_are_bounded_and_drain_on_error() -> io::Result<()> {
    futures::executor::block_on(async {
        for fail_id in [None, Some(3)] {
            let store = Arc::new(ReorderingKv {
                inner: MemoryKv::default(),
                active: AtomicUsize::new(0),
                maximum: AtomicUsize::new(0),
                finished: Mutex::default(),
                fail_id,
            });
            let snapshot =
                Snapshot::open_with_options(store.clone(), options(MapLayout::Radix)).await?;
            let result = snapshot
                .publish((0..40).map(|i| (i, Some(page(i)))).collect(), 40 * 4096)
                .await;
            assert_eq!(store.active.load(Ordering::SeqCst), 0);
            assert_eq!(store.maximum.load(Ordering::SeqCst), 8);
            assert!(
                store
                    .finished
                    .lock()
                    .unwrap()
                    .windows(2)
                    .any(|w| w[0] > w[1])
            );
            if fail_id.is_some() {
                assert!(result.is_err());
                assert!(store.inner.get(HEAD).await?.is_none());
            } else {
                result?;
                store.inner.crash(&[]);
                let snapshot = Snapshot::open(store).await?;
                for i in 0..40 {
                    assert_eq!(snapshot.block(i).await?, Some(page(i)));
                }
            }
        }
        Ok(())
    })
}

struct InterruptKv {
    inner: MemoryKv,
    calls: AtomicUsize,
    at: usize,
    effect: u8, // absent, visible only, durable
    cancel: bool,
    reached: Mutex<Option<futures::channel::oneshot::Sender<()>>>,
}
impl InterruptKv {
    async fn interrupt(&self) -> io::Result<()> {
        if self.cancel {
            let _ = self.reached.lock().unwrap().take().unwrap().send(());
            futures::future::pending::<()>().await;
        }
        Err(io::Error::other("injected fold failure"))
    }
}
impl KvStore for InterruptKv {
    fn get<'a>(&'a self, k: &'a str) -> BoxFuture<'a, io::Result<Option<Vec<u8>>>> {
        self.inner.get(k)
    }
    fn delete<'a>(&'a self, k: &'a str) -> BoxFuture<'a, io::Result<()>> {
        self.inner.delete(k)
    }
    fn put<'a>(&'a self, k: &'a str, v: Vec<u8>) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let fail = self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.at;
            if fail && self.effect == 1 {
                self.inner.simulate_unacknowledged_put(k, v);
            } else if !fail || self.effect == 2 {
                self.inner.put(k, v).await?;
            }
            if fail {
                self.interrupt().await?;
            }
            Ok(())
        })
    }
}

#[test]
fn every_override_fold_put_recovers_without_resurrecting_abandoned_changes() -> io::Result<()> {
    use futures::{
        FutureExt,
        channel::oneshot,
        future::{Either, select},
    };
    futures::executor::block_on(async {
        let base = Arc::new(MemoryKv::default());
        Snapshot::open_with_options(base.clone(), options(MapLayout::HeadOverrides))
            .await?
            .publish((0..300).map(|i| (i, Some(page(i)))).collect(), 300 * 4096)
            .await?;
        Snapshot::open(base.clone())
            .await?
            .publish(
                (0..64).map(|i| (i, Some(page(i + 1000)))).collect(),
                300 * 4096,
            )
            .await?;
        let old_head = base.get(HEAD).await?.unwrap();
        assert_eq!(Snapshot::open(base.clone()).await?.overrides.len(), 64);
        let probe = Arc::new(InterruptKv {
            inner: base.fork(),
            calls: AtomicUsize::new(0),
            at: usize::MAX,
            effect: 0,
            cancel: false,
            reached: Mutex::new(None),
        });
        Snapshot::open(probe.clone())
            .await?
            .publish(Changes::from([(256, Some(page(900)))]), 300 * 4096)
            .await?;
        let count = probe.calls.load(Ordering::SeqCst);
        let commit_at = count; // intent + object puts + publication head
        let new_head = probe.get(HEAD).await?.unwrap();
        for at in 1..=count {
            for effect in 0..3 {
                for cancel in [false, true] {
                    for persist_failed in [false, true] {
                        let (tx, rx) = oneshot::channel();
                        let fault = Arc::new(InterruptKv {
                            inner: base.fork(),
                            calls: AtomicUsize::new(0),
                            at,
                            effect,
                            cancel,
                            reached: Mutex::new(Some(tx)),
                        });
                        let snapshot = Snapshot::open(fault.clone()).await?;
                        let future =
                            snapshot.publish(Changes::from([(256, Some(page(900)))]), 300 * 4096);
                        if cancel {
                            match select(future.boxed(), rx).await {
                                Either::Left(_) => panic!("missed fold cancellation"),
                                Either::Right((signal, pending)) => {
                                    signal.unwrap();
                                    drop(pending);
                                }
                            }
                        } else {
                            let result = future.await;
                            if at <= commit_at {
                                assert!(result.is_err());
                            } else {
                                assert!(result?.cleanup_error.is_some());
                            }
                        }
                        drop(snapshot);
                        let extra = if persist_failed {
                            // At most one injected mutation is visible-only.
                            let mut keys: Vec<_> =
                                probe.inner.stats().write_keys.into_iter().collect();
                            keys.push(HEAD.to_owned());
                            keys
                        } else {
                            Vec::new()
                        };
                        fault.inner.crash(&extra);
                        let published = at > commit_at
                            || (at == commit_at
                                && (effect == 2 || (effect == 1 && persist_failed)));
                        // Recovery can finish a pending retirement list before head comparison.
                        drop(Snapshot::open(Arc::new(fault.inner.clone())).await?);
                        let head = fault.inner.get(HEAD).await?.unwrap();
                        assert_eq!(head, if published { &new_head } else { &old_head }.clone());
                        let expected = if published { 900 } else { 256 };
                        let snapshot = Snapshot::open(Arc::new(fault.inner.clone())).await?;
                        for i in 0..64 {
                            assert_eq!(snapshot.block(i).await?, Some(page(i + 1000)));
                        }
                        assert_eq!(snapshot.block(256).await?, Some(page(expected)));
                        // Retry modifies a different page. Abandoned page 256 must not reappear.
                        snapshot
                            .publish(Changes::from([(257, Some(page(901)))]), 300 * 4096)
                            .await?;
                        drop(snapshot);
                        fault.inner.crash(&[]);
                        let current = Snapshot::open(Arc::new(fault.inner.clone())).await?;
                        assert_eq!(current.block(256).await?, Some(page(expected)));
                        assert_eq!(current.block(257).await?, Some(page(901)));
                        drop(current);
                        assert_no_orphans(Arc::new(fault.inner.clone())).await?;
                        fault.inner.crash(&[]);
                        assert_eq!(
                            Snapshot::open(Arc::new(fault.inner.clone()))
                                .await?
                                .block(257)
                                .await?,
                            Some(page(901))
                        );
                    }
                }
            }
        }
        println!(
            "FOLD: {} crash/reopen cases across {count} puts",
            count * 12
        );
        Ok(())
    })
}
