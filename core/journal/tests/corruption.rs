//! Corrupted stored values: truncated, bit-flipped or garbage, at the
//! commit record, a stored log value, a page or the working-set hint. Open
//! and reads fail or read what was committed, never panic; the hint is only
//! advice, so whatever it holds, the journal reads as committed.
#![cfg(feature = "simulation")]
mod support;

use futures::FutureExt;
use journal::{Result, StorageOptions, kv::KvStore, simulation::*};
use std::{panic::AssertUnwindSafe, sync::Arc};
use support::*;

fn options() -> StorageOptions {
    StorageOptions {
        checkpoint_after: 2,
        inline_log_bytes: 200,
        prefetch_pages: 2,
        hint_pages: 8,
        ..StorageOptions::default()
    }
}

/// A namespace with every kind of key: checkpointed pages, a hint, stored
/// log values and records, and a log the record holds.
async fn committed() -> Result<(MemoryKv, Model)> {
    let memory = MemoryKv::default();
    let mut model = Model::default();
    let mut rng = Rng::new(0xbad);
    for _ in 0..5 {
        let session = Session::open(Arc::new(memory.clone()), options()).await?;
        for name in FILES {
            session.file(name).read(1).await?;
        }
        let ops: Vec<Op> = (0..10).map(|_| Op::random(&mut rng)).collect();
        for op in &ops {
            session.apply(op)?;
        }
        session.journal.commit().await?;
        model = model.applied(&ops);
    }
    Ok((memory, model))
}

/// Ways to corrupt a value: each one's name, and what it makes of it.
fn corruptions(value: &[u8], rng: &mut Rng) -> Vec<(String, Vec<u8>)> {
    let len = value.len();
    let mut out = vec![("empty".to_owned(), Vec::new())];
    for cut in [1, 4, len / 2, len.saturating_sub(1)] {
        if cut < len {
            out.push((format!("cut to {cut}"), value[..cut].to_vec()));
        }
    }
    let mut longer = value.to_vec();
    longer.extend(rng.bytes(3));
    out.push(("extended".into(), longer));
    let garbage = 1 + rng.below(2 * len as u64 + 8) as usize;
    let garbage = rng.bytes(garbage);
    out.push(("garbage".into(), garbage));
    for _ in 0..12 {
        let at = rng.below(len as u64) as usize;
        let mut flipped = value.to_vec();
        flipped[at] ^= 1 << rng.below(8);
        out.push((format!("bit flipped at {at}"), flipped));
    }
    out
}

/// What reading a corrupted namespace did.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Read {
    Failed,
    AsCommitted,
    Otherwise,
}

/// Open the namespace and read everything, without a cache or with one.
async fn read(store: Arc<dyn KvStore>, model: &Model, cached: bool) -> Read {
    let options = StorageOptions {
        cache: cached.then(fresh_namespace),
        ..options()
    };
    let read = async {
        let session = Session::open(store, options).await?;
        model.matches(&session).await
    };
    match read.await {
        Err(_) => Read::Failed,
        Ok(true) => Read::AsCommitted,
        Ok(false) => Read::Otherwise,
    }
}

#[test]
fn corrupted_values_fail_reads_or_read_as_committed_and_never_panic() -> Result<()> {
    futures::executor::block_on(async {
        let (memory, model) = committed().await?;
        let keys = memory.keys();
        for prefix in ["v15/commit", "v15/log/", "v15/page/", "v15/hint"] {
            assert!(
                keys.iter().any(|key| key.starts_with(prefix)),
                "{prefix}: {keys:?}"
            );
        }
        let mut rng = Rng::new(0xf11b);
        let mut outcomes = std::collections::BTreeMap::<_, usize>::new();
        for key in &keys {
            let value = memory.get(key).await?.unwrap();
            for (corruption, bytes) in corruptions(&value, &mut rng) {
                let corrupted = memory.fork();
                corrupted.put(key, bytes.into())?;
                for cached in [false, true] {
                    let case = format!("{key} {corruption}, cached {cached}");
                    let store: Arc<dyn KvStore> = Arc::new(corrupted.clone());
                    let read = AssertUnwindSafe(read(store, &model, cached)).catch_unwind();
                    let Ok(read) = read.await else {
                        panic!("{case}: panicked");
                    };
                    // A bit flip may stay undetected: only the log's frames
                    // of images have a checksum, not a record's value, a
                    // page or a segment's entries.
                    let flipped = corruption.starts_with("bit");
                    let kind = key.split('/').nth(1).unwrap();
                    match (&read, kind) {
                        (Read::AsCommitted, _) => {}
                        (Read::Failed, "commit" | "log" | "page") => {}
                        (Read::Otherwise, "commit" | "log" | "page") if flipped => {}
                        _ => panic!("{case}: {read:?}"),
                    }
                    *outcomes.entry((kind, flipped, read)).or_default() += 1;
                }
            }
        }
        println!("corrupted reads: {outcomes:?}");
        Ok(())
    })
}
