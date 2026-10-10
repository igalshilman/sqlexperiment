use super::*;
use crate::simulation::MemoryKv;
use futures::future::{Either, select};

async fn baseline() -> Result<MemoryKv> {
    let store = MemoryKv::default();
    let mut db = Session::open(Arc::new(store.clone())).await?;
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n INTEGER, payload TEXT)")
        .await?;
    db.execute("INSERT INTO t VALUES(1,10,printf('%09000d',1))")
        .await?;
    db.save().await?;
    store.crash(&[]);
    Ok(store)
}

async fn changed(store: MemoryKv) -> Result<Session> {
    let mut db = Session::open(Arc::new(store)).await?;
    db.execute("UPDATE t SET n=20, payload=printf('%09000d',2)")
        .await?;
    assert!(db.io_stats().private_wal_len > 0);
    Ok(db)
}

#[test]
fn interrupted_checkpoint_never_publishes() -> Result<()> {
    futures::executor::block_on(async {
        let base = baseline().await?;
        let probe = changed(base.fork()).await?;
        let io = probe.io.clone();
        let _unused = io.arm_fault(usize::MAX, false, false);
        probe.save().await?;
        let count = io.fault_ops();
        assert!(count > 0);
        for at in 1..=count {
            for after in [false, true] {
                for cancel in [false, true] {
                    let store = base.fork();
                    let db = changed(store.clone()).await?;
                    store.reset_stats();
                    let gate = db.io.arm_fault(at, after, cancel);
                    if cancel {
                        match select(db.save().boxed(), gate).await {
                            Either::Left(_) => panic!("checkpoint missed cancellation point"),
                            Either::Right((signal, pending)) => {
                                signal?;
                                drop(pending);
                            }
                        }
                    } else {
                        assert!(
                            db.save()
                                .await
                                .unwrap_err()
                                .to_string()
                                .contains("injected checkpoint")
                        );
                    }
                    assert_eq!(store.stats().puts, 0);
                    store.crash(&[]);
                    let mut db = Session::open(Arc::new(store)).await?;
                    assert_eq!(db.query("SELECT n FROM t").await?[0][0].to_string(), "10");
                    assert_eq!(
                        db.query("PRAGMA integrity_check").await?[0][0].to_string(),
                        "ok"
                    );
                }
            }
        }
        println!(
            "checkpoint: {} interruption cases across {count} local I/O operations",
            count * 4
        );
        Ok(())
    })
}

#[test]
fn publication_rejects_nonempty_private_wal() -> Result<()> {
    futures::executor::block_on(async {
        let store = baseline().await?;
        let db = changed(store.clone()).await?;
        store.reset_stats();
        assert!(
            db.io
                .publish()
                .await
                .unwrap_err()
                .to_string()
                .contains("WAL must be fully checkpointed")
        );
        assert_eq!(store.stats().puts, 0);
        Ok(())
    })
}
