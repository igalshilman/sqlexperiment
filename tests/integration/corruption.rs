//! Stored corruption is detected: a read errors or returns exactly what was
//! committed, never something else.
use sqlite_kv::{
    PageCompression, Result, Session, StorageOptions, kv::KvStore, simulation::MemoryKv,
};
use std::sync::Arc;

/// A database with checkpointed pages, a live log and a hint.
async fn populated(compression: PageCompression) -> Result<MemoryKv> {
    let store = MemoryKv::default();
    let options = StorageOptions {
        page_compression: compression,
        ..StorageOptions::default()
    };
    let mut db = Session::open_with_options(Arc::new(store.clone()), options.clone()).await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER, body BLOB)")
        .await?;
    db.save().await?;
    for round in 0..8 {
        let mut db = Session::open_with_options(Arc::new(store.clone()), options.clone()).await?;
        for i in 0..10 {
            let id = round * 10 + i;
            db.execute(&format!(
                "INSERT INTO t VALUES({id}, {id}, randomblob(300))"
            ))
            .await?;
        }
        db.query("SELECT count(*) FROM t").await?;
        db.save().await?;
    }
    Ok(store)
}

async fn contents(store: MemoryKv) -> Result<Vec<String>> {
    let mut db = Session::open(Arc::new(store)).await?;
    let rows = db
        .query("SELECT id, n, hex(body) FROM t ORDER BY id")
        .await?;
    let check = db.query("PRAGMA integrity_check").await?;
    if check[0][0].to_string() != "ok" {
        return Err("integrity check failed".into());
    }
    Ok(rows.iter().map(|row| format!("{row:?}")).collect())
}

#[test]
fn a_flipped_bit_is_an_error_or_harmless() -> Result<()> {
    futures::executor::block_on(async {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (mut detected, mut harmless) = (0, 0);
        for compression in [PageCompression::Raw, PageCompression::Lz4] {
            let store = populated(compression).await?;
            let expected = contents(store.fork()).await?;
            for key in store.keys() {
                let value = store.get(&key).await?.unwrap();
                for _ in 0..12 {
                    let bit = next() as usize % (value.len() * 8);
                    let mut flipped = value.to_vec();
                    flipped[bit / 8] ^= 1 << (bit % 8);
                    let copy = store.fork();
                    copy.put(&key, flipped.into()).await?;
                    match contents(copy).await {
                        Ok(rows) => {
                            assert_eq!(rows, expected, "{key} bit {bit}: silently wrong");
                            harmless += 1;
                        }
                        Err(_) => detected += 1,
                    }
                }
            }
        }
        println!("bit flips: {detected} detected, {harmless} harmless");
        assert!(detected > 100, "{detected} detected");
        Ok(())
    })
}
