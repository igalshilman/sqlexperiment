use sqlite_kv::{Result, Session, StorageOptions, simulation::MemoryKv};
use std::sync::Arc;

fn logged(max_log_segments: u32) -> StorageOptions {
    StorageOptions {
        max_log_segments,
        ..StorageOptions::default()
    }
}

async fn summary(db: &mut Session) -> Result<(i64, i64)> {
    let rows = db
        .query("SELECT count(*), coalesce(sum(id * length(payload)), 0) FROM t")
        .await?;
    Ok((rows[0][0].as_int().unwrap(), rows[0][1].as_int().unwrap()))
}

/// Many SQL sessions with appends, folds, a schema change, deletes and a policy
/// switch keep the same results as an in-memory model.
#[test]
fn sql_sessions_survive_appends_folds_and_policy_changes() -> Result<()> {
    futures::executor::block_on(async {
        let store = MemoryKv::default();
        let mut db = Session::open_with_options(Arc::new(store.clone()), logged(3)).await?;
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, payload TEXT)")
            .await?;
        db.save().await?;

        let segments =
            |store: &MemoryKv| store.keys().iter().filter(|k| k.contains("/log/")).count();
        let mut model = std::collections::BTreeMap::new();
        let mut appended = 0;
        for round in 0..19i64 {
            let mut db = Session::open_with_options(Arc::new(store.clone()), logged(3)).await?;
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
            assert!(segments(&store) <= 3);
            appended += usize::from(segments(&store) > 0);

            let expected = (
                model.len() as i64,
                model.iter().map(|(id, len)| id * len).sum::<i64>(),
            );
            let mut db = Session::open_with_options(Arc::new(store.clone()), logged(3)).await?;
            assert_eq!(summary(&mut db).await?, expected, "round {round}");
        }
        assert_eq!(appended, 15, "three appends per fold");
        assert_eq!(segments(&store), 3);

        // Switching the policy off folds the remaining segments.
        let mut db = Session::open(Arc::new(store.clone())).await?;
        let before = summary(&mut db).await?;
        db.execute("UPDATE t SET payload = payload || 'x' WHERE id = (SELECT min(id) FROM t)")
            .await?;
        db.save().await?;
        assert_eq!(segments(&store), 0);
        let mut db = Session::open(Arc::new(store.clone())).await?;
        let after = summary(&mut db).await?;
        assert_eq!(after.0, before.0);
        let rows = db
            .query("SELECT count(*) FROM t INDEXED BY by_payload WHERE payload > ''")
            .await?;
        assert_eq!(rows[0][0].as_int(), Some(after.0));
        Ok(())
    })
}
