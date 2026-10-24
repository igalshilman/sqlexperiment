//! Corrupted stored values: truncated, bit-flipped or garbage, at the root
//! or a blob. Open and reads fail or read what was flushed, never panic.
#![cfg(feature = "simulation")]
mod support;

use futures::FutureExt;
use state::{Result, StateOptions, kv::KvStore, simulation::*};
use std::{panic::AssertUnwindSafe, sync::Arc};
use support::*;

fn options() -> StateOptions {
    StateOptions {
        chunk_pages: 2,
        rebase_ratio: 0.5,
        inline_record_bytes: 200,
        prefetch_pages: 2,
        ..StateOptions::default()
    }
}

/// A namespace with every kind of key: the root, and blobs of chunks (bases
/// and deltas) and of records.
async fn flushed() -> Result<(MemoryKv, Model)> {
    let memory = MemoryKv::default();
    let mut model = Model::default();
    let mut rng = Rng::new(0xbad);
    for _ in 0..5 {
        let session = Session::open(Arc::new(memory.clone()), options()).await?;
        for name in FILES {
            session.file(name).read(1).await?;
        }
        let ops: Vec<Op> = (0..10).map(|_| Op::random(&mut rng)).collect();
        session.flush(&ops).await?;
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
    AsFlushed,
    Otherwise,
}

/// Open the namespace and read everything, without a cache or with one.
async fn read(store: Arc<dyn KvStore>, model: &Model, cached: bool) -> Read {
    let options = Setup {
        options: options(),
        cache: cached.then(fresh_namespace),
    };
    let read = async {
        let session = Session::open(store, options).await?;
        model.matches(&session).await
    };
    match read.await {
        Err(_) => Read::Failed,
        Ok(true) => Read::AsFlushed,
        Ok(false) => Read::Otherwise,
    }
}

#[test]
fn corrupted_values_fail_reads_or_read_as_flushed_and_never_panic() -> Result<()> {
    futures::executor::block_on(async {
        let (memory, model) = flushed().await?;
        let keys = memory.keys();
        for prefix in ["v16/root", "v16/blob/"] {
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
                    // A bit flip may stay undetected: frames have a
                    // checksum, but not their headers.
                    let flipped = corruption.starts_with("bit");
                    let kind = key.split('/').nth(1).unwrap();
                    match &read {
                        Read::AsFlushed | Read::Failed => {}
                        Read::Otherwise if flipped => {}
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
