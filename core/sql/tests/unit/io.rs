use super::*;
use state::simulation::MemoryKv;

#[test]
fn cached_snapshot_page_never_masks_overlay_wal_or_truncation() -> crate::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let initial = KvIo::mount(store.clone()).await?;
        initial.write(0, 0, &vec![7; 2 * BLOCK_SIZE]).await?;
        initial.publish().await?;
        let io = KvIo::mount(store.clone()).await?;
        io.prepare_engine().await?;
        assert_eq!(*io.block(0, 0).await?, vec![7; BLOCK_SIZE]);
        io.write(0, 10, &[9, 8]).await?;
        assert_eq!(&io.block(0, 0).await?[10..12], &[9, 8]);
        assert_eq!(*io.block(1, 0).await?, vec![0; BLOCK_SIZE]);
        io.write(1, 0, &[3]).await?;
        assert_eq!(io.block(1, 0).await?[0], 3);
        assert_eq!(io.block(0, 0).await?[0], 7);
        assert_eq!(*io.block(0, 1).await?, vec![7; BLOCK_SIZE]);
        io.truncate_file(0, 100).await?;
        io.truncate_file(0, (2 * BLOCK_SIZE) as u64).await?;
        let page = io.block(0, 0).await?;
        assert_eq!(&page[10..12], &[9, 8]);
        assert!(page[100..].iter().all(|&b| b == 0));
        assert_eq!(*io.block(0, 1).await?, vec![0; BLOCK_SIZE]);
        io.truncate_file(1, 0).await?;
        io.publish().await?;
        let reopened = KvIo::mount(store).await?;
        assert_eq!(reopened.block(0, 0).await?, page);
        assert_eq!(*reopened.block(0, 1).await?, vec![0; BLOCK_SIZE]);
        Ok(())
    })
}

#[test]
fn callbacks_run_on_host_and_shutdown_aborts_queued_requests() -> crate::Result<()> {
    futures::executor::block_on(async {
        for abort in [false, true] {
            let io = KvIo::mount(Arc::new(MemoryKv::default())).await?;
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
            assert!(!c.finished());
            assert_eq!(io.pending_count(), 1);
            if abort {
                io.shutdown();
                assert!(c.failed());
                assert!(!called_on.lock().unwrap().unwrap().1);
            } else {
                io.drive().await?;
                assert_eq!(
                    *called_on.lock().unwrap(),
                    Some((std::thread::current().id(), true))
                );
                assert!(c.succeeded());
            }
            assert_eq!(io.pending_count(), 0);
        }
        Ok(())
    })
}

#[test]
fn synchronous_engine_waits_use_only_resident_data_or_fail() -> crate::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let initial = KvIo::mount(store.clone()).await?;
        initial.write(0, 0, &vec![7; 2 * BLOCK_SIZE]).await?;
        initial.publish().await?;
        let io = KvIo::mount(store.clone()).await?;
        io.prepare_engine().await?;
        let file = MountedIo(io.clone()).open_file(DB_PATH, OpenFlags::Create, false)?;
        store.reset_stats();
        let out = Arc::new(Buffer::new_temporary(BLOCK_SIZE));
        let header = file.pread(0, Completion::new_read(out.clone(), |_| None))?;
        io.step()?;
        assert!(header.succeeded());
        assert!(!io.failed(), "a resident read is no failure");
        assert!(out.as_slice().iter().all(|&b| b == 7));
        assert_eq!(store.stats().gets, 0);
        out.as_mut_slice().fill(9);
        let cold = file.pread(
            BLOCK_SIZE as u64,
            Completion::new_read(out.clone(), |_| None),
        )?;
        assert!(io.step().unwrap_err().to_string().contains("nonresident"));
        assert!(cold.failed());
        // The session must not take the engine's error for an SQL error.
        assert!(io.failed(), "a failed synchronous read is recorded");
        assert!(out.as_slice().iter().all(|&b| b == 9));
        assert_eq!(
            store.stats().gets,
            0,
            "synchronous paths must not start KV requests"
        );
        let write = file.pwrite(0, out, Completion::new_write(|_| {}))?;
        assert!(
            io.step()
                .unwrap_err()
                .to_string()
                .contains("synchronous file mutation")
        );
        assert!(write.failed());
        assert_eq!(io.pending_count(), 0);
        assert!(io.step().is_err(), "an unserviceable wait must not spin");
        Ok(())
    })
}
