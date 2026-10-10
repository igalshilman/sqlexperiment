use super::*;
use crate::io::BLOCK_SIZE;
use crate::simulation::MemoryKv;
#[test]
fn height_grows_shrinks_and_small_reads_need_no_external_map_node() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let snapshot = Snapshot::open(store.clone()).await?;
        snapshot
            .publish(
                BTreeMap::from([
                    (0, Some(vec![1; BLOCK_SIZE])),
                    (255, Some(vec![2; BLOCK_SIZE])),
                ]),
                256 * BLOCK_SIZE as u64,
            )
            .await?;
        let mut snapshot = Snapshot::open(store.clone()).await?;
        assert_eq!(snapshot.head.height, 1);
        for (index, expected) in [(256, 2), (65536, 3), (1 << 40, 6)] {
            assert!(snapshot.block(index).await?.is_none());
            snapshot
                .publish(
                    BTreeMap::from([(index, Some(vec![7; BLOCK_SIZE]))]),
                    (index + 1) * BLOCK_SIZE as u64,
                )
                .await?;
            snapshot = Snapshot::open(store.clone()).await?;
            assert_eq!(snapshot.head.height, expected);
            assert_eq!(snapshot.block(index).await?, Some(vec![7; BLOCK_SIZE]));
            assert_eq!(snapshot.block(0).await?, Some(vec![1; BLOCK_SIZE]));
            assert!(snapshot.block(index + 1).await?.is_none());
        }
        snapshot
            .publish(
                BTreeMap::from([(256, None), (65536, None), (1 << 40, None)]),
                256 * BLOCK_SIZE as u64,
            )
            .await?;
        store.reset_stats();
        let snapshot = Snapshot::open(store.clone()).await?;
        assert_eq!(snapshot.head.height, 1);
        assert_eq!(snapshot.block(255).await?, Some(vec![2; BLOCK_SIZE]));
        assert_eq!(
            store.stats().gets,
            3,
            "pending check + head containing root + one block"
        );
        assert!(snapshot.block(256).await?.is_none());
        assert_eq!(store.stats().gets, 3, "outside tree range requires no read");
        snapshot
            .publish(BTreeMap::from([(0, None), (255, None)]), 0)
            .await?;
        let empty = Snapshot::open(store).await?;
        assert_eq!(empty.head.height, 0);
        assert!(empty.head.root.is_empty());
        Ok(())
    })
}

#[test]
fn persistent_tree_sparse_paths_updates_and_deletions() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let mut snapshot = Snapshot::open(store.clone()).await?;
        let indexes = [0, 1, 255, 256, 257, 65535, 65536, 1 << 40];
        let mut dirty = Changes::new();
        for (i, &index) in indexes.iter().enumerate() {
            dirty.insert(index, Some(vec![i as u8 + 1; BLOCK_SIZE]));
        }
        snapshot
            .publish(dirty, ((1 << 40) + 1) * BLOCK_SIZE as u64)
            .await?;
        snapshot = Snapshot::open(store.clone()).await?;
        assert!(snapshot.block(99).await?.is_none());
        for (i, &index) in indexes.iter().enumerate() {
            assert_eq!(
                snapshot.block(index).await?.unwrap(),
                vec![i as u8 + 1; BLOCK_SIZE]
            );
        }
        let mut dirty = Changes::new();
        dirty.insert(256, None);
        dirty.insert(255, Some(vec![99; BLOCK_SIZE]));
        dirty.insert(258, Some(vec![42; BLOCK_SIZE]));
        snapshot.publish(dirty, snapshot.head.size).await?;
        let updated = Snapshot::open(store.clone()).await?;
        assert!(updated.block(256).await?.is_none());
        assert_eq!(updated.block(255).await?.unwrap(), vec![99; BLOCK_SIZE]);
        assert_eq!(updated.block(258).await?.unwrap(), vec![42; BLOCK_SIZE]);
        // Saving invalidates old snapshots; their replaced blocks are reclaimed.
        assert!(snapshot.block(255).await.is_err());
        let mut dirty: Changes = indexes.iter().map(|&i| (i, None)).collect();
        dirty.insert(258, None);
        updated.publish(dirty, 0).await?;
        let empty = Snapshot::open(store).await?;
        assert!(empty.head.root.is_empty());
        assert_eq!(empty.head.size, 0);
        Ok(())
    })
}

#[test]
fn referenced_objects_must_exist_and_have_valid_structure() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let snapshot = Snapshot::open(store.clone()).await?;
        let bytes = vec![7; BLOCK_SIZE];
        snapshot
            .publish(
                BTreeMap::from([(0, Some(bytes.clone())), (256, Some(bytes.clone()))]),
                257 * BLOCK_SIZE as u64,
            )
            .await?;
        let block_key = Kind::Block.key(1);
        store.delete(&block_key).await?;
        let snapshot = Snapshot::open(store.clone()).await?;
        assert!(
            snapshot
                .block(0)
                .await
                .unwrap_err()
                .to_string()
                .contains("missing referenced")
        );
        store.put(&block_key, vec![8; BLOCK_SIZE - 1]).await?;
        assert!(
            snapshot
                .block(0)
                .await
                .unwrap_err()
                .to_string()
                .contains("invalid")
        );
        let root_key = Kind::Node.key(snapshot.head.root[&0]);
        store.put(&root_key, vec![0; 9]).await?;
        let snapshot = Snapshot::open(store.clone()).await?;
        assert!(
            snapshot
                .block(0)
                .await
                .unwrap_err()
                .to_string()
                .contains("invalid")
        );
        store.put(HEAD, b"bad head".to_vec()).await?;
        assert!(Snapshot::open(store.clone()).await.is_err());
        Ok(())
    })
}
