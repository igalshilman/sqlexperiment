use super::*;
use crate::simulation::MemoryKv;

fn head(high: u64) -> Vec<u8> {
    let mut h = b"SK10".to_vec();
    h.extend(0u64.to_le_bytes());
    h.extend(high.to_le_bytes());
    h.push(0);
    h
}
#[test]
fn malformed_intents_and_unexpected_heads_fail_before_mutation() -> io::Result<()> {
    futures::executor::block_on(async {
        let intent = PendingSave {
            old_head: Some(head(10)),
            new_head: head(12),
            created: Objects::from([("block", 11), ("node", 12)]),
            retired: Objects::from([("block", 2)]),
        };
        let valid = intent.encode()?;
        let mut bad: Vec<_> = (0..valid.len()).map(|n| valid[..n].to_vec()).collect();
        let mut b = valid.clone();
        b[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        bad.push(b);
        let mut b = valid.clone();
        b[20 + head(10).len() + head(12).len()] = 9;
        bad.push(b); // invalid object kind
        let mut b = valid.clone();
        b[20 + head(10).len() + head(12).len() + 1..20 + head(10).len() + head(12).len() + 9]
            .copy_from_slice(&1u64.to_le_bytes());
        bad.push(b); // created ID in committed range
        let mut b = valid.clone();
        let n = b.len();
        b[n - 8..].copy_from_slice(&11u64.to_le_bytes());
        bad.push(b); // retire new ID
        for bytes in bad {
            let store = Arc::new(MemoryKv::default());
            store.put(HEAD, head(10)).await?;
            store.put(PENDING, bytes).await?;
            store.reset_stats();
            assert!(recover(store.clone()).await.is_err());
            assert_eq!(store.stats().puts + store.stats().deletes, 0);
        }
        let store = Arc::new(MemoryKv::default());
        store.put(HEAD, head(99)).await?;
        store.put(PENDING, valid).await?;
        store.reset_stats();
        assert!(
            recover(store.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        assert_eq!(store.stats().puts + store.stats().deletes, 0);
        Ok(())
    })
}
#[test]
fn large_intent_has_an_explicit_limit() {
    let intent = PendingSave {
        old_head: None,
        new_head: head(120000),
        created: (1..=120000).map(|id| ("block", id)).collect(),
        retired: Objects::new(),
    };
    assert!(intent.encode().unwrap_err().to_string().contains("1 MiB"));
}
