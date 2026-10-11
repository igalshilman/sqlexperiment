use super::*;
use crate::simulation::MemoryKv;
use futures::future::{join_all, try_join};

#[test]
fn overlapping_requests_share_a_round_trip() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = LatencyKv::new(MemoryKv::default());
        store.put("a", vec![1].into()).await?;
        store.get("a").await?;
        assert_eq!(store.round_trips(), 2, "dependent requests");
        store.reset();
        join_all((0..10).map(|n| store.get(if n % 2 == 0 { "a" } else { "b" }))).await;
        assert_eq!(store.round_trips(), 1, "one batch");
        store.reset();
        try_join(store.get("a"), async {
            store.get("a").await?;
            store.delete("a").await
        })
        .await?;
        assert_eq!(store.round_trips(), 2, "the longest chain");
        Ok(())
    })
}
