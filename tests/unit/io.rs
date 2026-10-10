use super::*;
use crate::simulation::MemoryKv;

#[test]
fn cached_snapshot_page_never_masks_overlay_wal_or_truncation() -> crate::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let initial = KvIo::mount(store.clone()).await?;
        initial.write(0, 0, &vec![7; 2 * BLOCK_SIZE]).await?;
        initial.publish().await?;
        let io = KvIo::mount(store.clone()).await?;
        assert_eq!(io.block(0, 0).await?, vec![7; BLOCK_SIZE]);
        io.write(0, 10, &[9, 8]).await?;
        assert_eq!(&io.block(0, 0).await?[10..12], &[9, 8]);
        assert_eq!(io.block(1, 0).await?, vec![0; BLOCK_SIZE]);
        io.write(1, 0, &[3]).await?;
        assert_eq!(io.block(1, 0).await?[0], 3);
        assert_eq!(io.block(0, 0).await?[0], 7);
        assert_eq!(io.block(0, 1).await?, vec![7; BLOCK_SIZE]);
        io.truncate_file(0, 100).await?;
        io.truncate_file(0, (2 * BLOCK_SIZE) as u64).await?;
        let page = io.block(0, 0).await?;
        assert_eq!(&page[10..12], &[9, 8]);
        assert!(page[100..].iter().all(|&b| b == 0));
        assert_eq!(io.block(0, 1).await?, vec![0; BLOCK_SIZE]);
        io.truncate_file(1, 0).await?;
        io.publish().await?;
        let reopened = KvIo::mount(store).await?;
        assert_eq!(reopened.block(0, 0).await?, page);
        assert_eq!(reopened.block(0, 1).await?, vec![0; BLOCK_SIZE]);
        Ok(())
    })
}

#[test]
fn engine_callbacks_wait_for_worker_step_and_shutdown_aborts_ready() -> crate::Result<()> {
    futures::executor::block_on(async {
        for abort in [false, true] {
            let io = KvIo::mount(Arc::new(MemoryKv::default())).await?;
            io.use_worker_completions();
            let file = MountedIo(io.clone()).open_file(DB_PATH, OpenFlags::Create, false)?;
            let called_on = Arc::new(Mutex::new(None));
            let captured = called_on.clone();
            let c = file.pwrite(
                0,
                Arc::new(Buffer::new_temporary(4096)),
                Completion::new_write(move |result| {
                    *captured.lock().unwrap() = Some((std::thread::current().id(), result.is_ok()));
                }),
            )?;
            io.drive().await?;
            assert!(!c.finished(), "host must not run engine success callbacks");
            assert_eq!(
                io.pending_count(),
                1,
                "queued completion still counts as in flight"
            );
            if abort {
                io.shutdown();
                assert!(c.failed());
                assert!(!called_on.lock().unwrap().unwrap().1);
            } else {
                let worker_io = io.clone();
                let worker = std::thread::spawn(move || {
                    worker_io.step().unwrap();
                    std::thread::current().id()
                });
                let worker_id = worker.join().unwrap();
                assert_eq!(*called_on.lock().unwrap(), Some((worker_id, true)));
                assert!(c.succeeded());
            }
            assert_eq!(io.pending_count(), 0);
        }
        Ok(())
    })
}
