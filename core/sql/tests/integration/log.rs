use crate::host::Db;
use journal::{Result, StorageOptions, simulation::MemoryKv};
use std::sync::Arc;

fn options(checkpoint_after: u32) -> StorageOptions {
    StorageOptions {
        checkpoint_after,
        ..StorageOptions::default()
    }
}

async fn summary(db: &mut Db) -> Result<(i64, i64)> {
    let rows = db
        .query("SELECT count(*), coalesce(sum(id * length(payload)), 0) FROM t")
        .await?;
    Ok((rows[0][0].as_int().unwrap(), rows[0][1].as_int().unwrap()))
}

/// Many SQL sessions with inserts, deletes, a schema change and changing
/// checkpoint policies keep the same results as an in-memory model, while the
/// log stays bounded.
#[test]
fn sql_sessions_match_a_model_across_checkpoint_policies() -> Result<()> {
    futures::executor::block_on(async {
        let store = MemoryKv::default();
        let mut db = Db::open(Arc::new(store.clone()), options(3)).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, payload TEXT)")
            .await?;
        db.save().await?;

        let segments =
            |store: &MemoryKv| store.keys().iter().filter(|k| k.contains("/log/")).count();
        let mut model = std::collections::BTreeMap::new();
        for round in 0..24i64 {
            let policy = [3, 3, 0, 8][(round / 6) as usize];
            let mut db = Db::open(Arc::new(store.clone()), options(policy)).await?;
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
            db.save().await?;
            // The log, plus garbage awaiting collection, stays bounded.
            assert!(
                segments(&store) <= 2 * (policy as usize + 2),
                "round {round}"
            );

            let expected = (
                model.len() as i64,
                model.iter().map(|(id, len)| id * len).sum::<i64>(),
            );
            let mut db = Db::open(Arc::new(store.clone()), options(policy)).await?;
            assert_eq!(summary(&mut db).await?, expected, "round {round}");
        }
        let mut db = Db::open(Arc::new(store.clone()), StorageOptions::default()).await?;
        let rows = db
            .query("SELECT count(*) FROM t INDEXED BY by_payload WHERE payload > ''")
            .await?;
        assert_eq!(rows[0][0].as_int(), Some(model.len() as i64));
        assert!(store.page_count() > 0, "checkpoints wrote pages in place");
        Ok(())
    })
}
