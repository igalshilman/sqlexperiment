use crate::host::Db;
use state::{Result, StateOptions, simulation::MemoryKv};
use std::sync::Arc;

fn options(garbage_after: u64) -> StateOptions {
    StateOptions {
        garbage_after,
        ..StateOptions::default()
    }
}

/// Blobs the current root names: at most one base and one delta per chunk
/// of the database, and one per record.
async fn live(db: &mut Db) -> Result<usize> {
    let pages = db.query("PRAGMA page_count").await?[0][0].as_int().unwrap() as usize;
    Ok(2 * pages.div_ceil(state::CHUNK_PAGES as usize) + 8)
}

async fn summary(db: &mut Db) -> Result<(i64, i64)> {
    let rows = db
        .query("SELECT count(*), coalesce(sum(id * length(payload)), 0) FROM t")
        .await?;
    Ok((rows[0][0].as_int().unwrap(), rows[0][1].as_int().unwrap()))
}

/// Many SQL sessions with inserts, deletes, a schema change and changing
/// garbage graces keep the same results as an in-memory model, while the
/// blobs stay bounded: the live ones, and the garbage of the last flushes.
#[test]
fn sql_sessions_match_a_model_across_garbage_graces() -> Result<()> {
    futures::executor::block_on(async {
        let store = MemoryKv::default();
        let mut db = Db::open(Arc::new(store.clone()), options(3)).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, payload TEXT)")
            .await?;
        db.save().await?;

        let mut written = std::collections::VecDeque::new();
        let mut model = std::collections::BTreeMap::new();
        for round in 0..24i64 {
            let grace = [3, 3, 1, 8][(round / 6) as usize];
            let mut db = Db::open(Arc::new(store.clone()), options(grace)).await?;
            let start = round * 25;
            db.execute(&format!(
                "WITH RECURSIVE n(x) AS (VALUES({start}) UNION ALL SELECT x+1 FROM n WHERE x<{}) \
                 INSERT INTO t SELECT x, printf('%0{}d', x) FROM n",
                start + 24,
                50 + round
            ))
            .await?;
            for id in start..start + 25 {
                model.insert(id, 50 + round);
            }
            if round % 4 == 3 {
                db.execute(&format!("DELETE FROM t WHERE id % 7 = {}", round % 7))
                    .await?;
                model.retain(|id, _| id % 7 != round % 7);
            }
            if round == 10 {
                db.execute("CREATE INDEX by_payload ON t(payload)").await?;
            }
            let before = store.blob_count();
            db.save().await?;
            // Each flush writes blobs; garbage goes once `grace` more
            // flushes landed, so what is kept is the live blobs and those
            // written by the last `grace` flushes (plus slack for a lowered
            // grace).
            written.push_front(store.blob_count().saturating_sub(before) + 1);
            let recent: usize = written.iter().take(8 + 1).sum();

            let expected = (
                model.len() as i64,
                model.iter().map(|(id, len)| id * len).sum::<i64>(),
            );
            let mut db = Db::open(Arc::new(store.clone()), options(grace)).await?;
            assert_eq!(summary(&mut db).await?, expected, "round {round}");
            let blobs = store.blob_count();
            assert!(
                blobs <= live(&mut db).await? + recent,
                "round {round}: {blobs} blobs"
            );
        }
        let mut db = Db::open(Arc::new(store.clone()), StateOptions::default()).await?;
        let rows = db
            .query("SELECT count(*) FROM t INDEXED BY by_payload WHERE payload > ''")
            .await?;
        assert_eq!(rows[0][0].as_int(), Some(model.len() as i64));
        Ok(())
    })
}
