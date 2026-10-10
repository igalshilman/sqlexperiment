//! Operation/byte benchmark. MemoryKv is a simulator, not a remote durability benchmark.
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use turso_lazy_kv::{
    MapLayout, Result, Session, StorageOptions,
    kv::{KvStore, MemoryKv, Stats},
};

#[derive(Clone, Copy)]
struct Case {
    rows: usize,
    payload: usize,
    indexes: usize,
}
async fn baseline(case: Case, layout: MapLayout) -> Result<MemoryKv> {
    let store = MemoryKv::default();
    let mut db = Session::open_with_options(
        Arc::new(store.clone()),
        StorageOptions {
            layout,
            write_concurrency: 16,
        },
    )
    .await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, c INTEGER, note INTEGER, payload TEXT)").await?;
    for column in ["a", "b", "c"].into_iter().take(case.indexes) {
        db.execute(&format!("CREATE INDEX by_{column} ON t({column})"))
            .await?;
    }
    db.execute(&format!("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<{}) INSERT INTO t SELECT x, x, x, x, 0, printf('%0{}d',x) FROM n", case.rows, case.payload)).await?;
    assert!(db.save().await?.cleanup_error.is_none());
    store.crash(&[]);
    Ok(store)
}
fn print_row(
    case: Case,
    layout: MapLayout,
    op: &str,
    run: usize,
    sizes: (u64, usize, usize),
    elapsed: Duration,
    s: &Stats,
) {
    let (db_bytes, before, after) = sizes;
    println!(
        "{layout:?},{},{},{},{op},{run},{db_bytes},{before},{after},{},{},{},{},{},{},{},{},{},{}",
        case.rows,
        case.payload,
        case.indexes,
        elapsed.as_micros(),
        s.gets,
        s.puts,
        s.deletes,
        s.bytes_read,
        s.bytes_written,
        s.block_bytes_written,
        s.node_bytes_written,
        s.head_bytes_written,
        s.intent_bytes_written
    );
}
async fn run(case: Case, layout: MapLayout, repeats: usize) -> Result<()> {
    let base = baseline(case, layout).await?;
    let head = base.get("v8/head").await?.unwrap();
    let db_bytes = u64::from_le_bytes(head[4..12].try_into().unwrap());
    let before = base.stored_bytes();
    let id = case.rows / 2;
    let query = format!("SELECT id, length(payload) FROM t WHERE id={id}");
    for repeat in 0..repeats {
        for op in [
            "cold_pk",
            "warm_pk",
            "secondary_lookup",
            "update_note",
            "update_a",
            "insert",
            "delete",
            "ten_updates_one_save",
            "ten_updates_ten_saves",
        ] {
            if op == "secondary_lookup" && case.indexes == 0 {
                continue;
            }
            let store = Arc::new(base.fork());
            let mut warm = if op == "warm_pk" {
                let mut db = Session::open(store.clone()).await?;
                db.query(&query).await?;
                Some(db)
            } else {
                None
            };
            store.reset_stats();
            let start = Instant::now();
            let mut db = match warm.take() {
                Some(db) => db,
                None => Session::open(store.clone()).await?,
            };
            match op {
                "cold_pk" | "warm_pk" | "secondary_lookup" => {
                    let sql = if op == "secondary_lookup" {
                        format!("SELECT id, length(payload) FROM t WHERE a={id}")
                    } else {
                        query.clone()
                    };
                    let rows = db.query(&sql).await?;
                    assert_eq!(rows[0][0].to_string(), id.to_string());
                    assert_eq!(rows[0][1].to_string(), case.payload.to_string());
                    let elapsed = start.elapsed();
                    print_row(
                        case,
                        layout,
                        op,
                        repeat,
                        (db_bytes, before, store.stored_bytes()),
                        elapsed,
                        &store.stats(),
                    );
                    // Drop happens outside the read timing.
                    drop(db);
                    continue;
                }
                "update_note" => {
                    db.execute(&format!("UPDATE t SET note=note+1 WHERE id={id}"))
                        .await?
                }
                "update_a" => {
                    db.execute(&format!("UPDATE t SET a=a+1000000 WHERE id={id}"))
                        .await?
                }
                "insert" => {
                    db.execute(&format!(
                        "INSERT INTO t VALUES({},999999,999999,999999,0,printf('%0{}d',1))",
                        case.rows + 1,
                        case.payload
                    ))
                    .await?
                }
                "delete" => db.execute(&format!("DELETE FROM t WHERE id={id}")).await?,
                "ten_updates_one_save" | "ten_updates_ten_saves" => {
                    for n in 1..=10 {
                        db.execute(&format!("UPDATE t SET note={n} WHERE id={id}"))
                            .await?;
                        if op == "ten_updates_ten_saves" && n < 10 {
                            assert!(db.save().await?.cleanup_error.is_none());
                            db = Session::open(store.clone()).await?;
                        }
                    }
                }
                _ => unreachable!(),
            }
            assert!(db.save().await?.cleanup_error.is_none());
            let elapsed = start.elapsed();
            let stats = store.stats();
            print_row(
                case,
                layout,
                op,
                repeat,
                (db_bytes, before, store.stored_bytes()),
                elapsed,
                &stats,
            );
            // Validate results after timing/count capture, with simulated loss of
            // all unacknowledged state and a fresh engine.
            store.crash(&[]);
            let mut reopened = Session::open(store.clone()).await?;
            let count = reopened.query("SELECT count(*) FROM t").await?[0][0].to_string();
            let expected_count =
                case.rows + usize::from(op == "insert") - usize::from(op == "delete");
            assert_eq!(count, expected_count.to_string());
            if op == "update_note" || op.starts_with("ten_updates") {
                let n = reopened
                    .query(&format!("SELECT note FROM t WHERE id={id}"))
                    .await?[0][0]
                    .to_string();
                assert_eq!(n, if op == "update_note" { "1" } else { "10" });
            }
            if op == "update_a" {
                assert_eq!(
                    reopened
                        .query(&format!("SELECT a FROM t WHERE id={id}"))
                        .await?[0][0]
                        .to_string(),
                    (id + 1000000).to_string()
                );
            }
            assert_eq!(
                reopened.query("PRAGMA integrity_check").await?[0][0].to_string(),
                "ok"
            );
            assert!(store.get("v8/pending_save").await?.is_none());
        }
    }
    Ok(())
}
fn main() -> Result<()> {
    let repeats = std::env::args()
        .nth(1)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(3);
    if repeats == 0 {
        return Err("repeats must be positive".into());
    }
    println!(
        "layout,rows,payload_bytes,indexes,operation,run,db_bytes,stored_before,stored_after,elapsed_us,gets,puts,deletes,bytes_read,bytes_written,block_bytes,node_bytes,head_bytes,intent_bytes"
    );
    futures::executor::block_on(async {
        for layout in [
            MapLayout::Radix,
            MapLayout::InlineRoot,
            MapLayout::HeadOverrides,
        ] {
            for rows in [10, 100, 300] {
                for payload in [64, 1024] {
                    for indexes in [0, 1, 3] {
                        run(
                            Case {
                                rows,
                                payload,
                                indexes,
                            },
                            layout,
                            repeats,
                        )
                        .await?;
                    }
                }
            }
            run(
                Case {
                    rows: 10000,
                    payload: 256,
                    indexes: 1,
                },
                layout,
                repeats,
            )
            .await?;
        }
        Ok(())
    })
}
