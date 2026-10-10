use super::*;

#[test]
fn successful_puts_and_deletes_survive_immediate_crash() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = MemoryKv::default();
        for value in [vec![1; 4096], vec![2; 37]] {
            store.put("key", value.clone()).await?;
            store.crash(&[]);
            assert_eq!(store.get("key").await?, Some(value));
        }
        for _ in 0..2 {
            store.delete("key").await?;
            store.crash(&[]);
            assert!(store.get("key").await?.is_none());
        }
        Ok(())
    })
}

#[test]
fn unknown_outcomes_are_atomic_and_not_flushed_by_unrelated_success() -> io::Result<()> {
    futures::executor::block_on(async {
        for persist in [false, true] {
            let store = MemoryKv::default();
            store.put("head", vec![1; 100]).await?;
            store.put("garbage", vec![3]).await?;
            store.put("deleted", vec![4]).await?;
            store.simulate_unacknowledged_put("head", vec![2; 200]);
            store.simulate_unacknowledged_delete("garbage");
            assert_eq!(store.get("head").await?, Some(vec![2; 200]));
            assert!(store.get("garbage").await?.is_none());
            store.put("acknowledged", vec![5]).await?;
            store.delete("deleted").await?;
            let extra = if persist {
                vec!["head".into(), "garbage".into()]
            } else {
                vec![]
            };
            store.crash(&extra);
            assert_eq!(
                store.get("head").await?,
                Some(if persist { vec![2; 200] } else { vec![1; 100] })
            );
            assert_eq!(
                store.get("garbage").await?,
                if persist { None } else { Some(vec![3]) }
            );
            assert_eq!(store.get("acknowledged").await?, Some(vec![5]));
            assert!(store.get("deleted").await?.is_none());
        }
        Ok(())
    })
}
