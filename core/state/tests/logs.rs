//! Logs: entries appended in a mutation are read once flushed, from the
//! index each took, and survive a reopen; a truncation drops a prefix, the
//! entries the same mutation appends included, and its blobs become garbage;
//! a write refuses what no flush could store; a reader keeps the log it
//! opened.
#![cfg(feature = "simulation")]
use bytes::Bytes;
use state::{
    MAX_LOG_ENTRIES, MAX_RECORD_BYTES, Result, State, StateOptions, kv::KvStore, simulation::*,
};
use std::sync::Arc;

fn entries(values: &[&str]) -> Vec<Bytes> {
    values
        .iter()
        .map(|value| Bytes::from(value.to_string()))
        .collect()
}

/// An entry of a few KiB.
fn large(n: u8) -> Bytes {
    vec![n; 2000].into()
}

#[test]
fn appended_entries_are_read_once_flushed_and_survive_a_reopen() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let log = state.log("turns");
        assert_eq!((log.first(), log.next(), log.entries()), (0, 0, vec![]));
        let mut mutation = state.mutation();
        assert_eq!(mutation.append(&log, "a".into())?, 0);
        assert_eq!(mutation.append(&log, large(1))?, 1);
        assert_eq!(log.next(), 0, "not flushed yet");
        mutation.flush().await?;
        let mut mutation = state.mutation();
        assert_eq!(mutation.append(&log, "c".into())?, 2);
        mutation.flush().await?;
        let expected = vec!["a".into(), large(1), "c".into()];
        assert_eq!(
            (log.first(), log.next(), log.entries()),
            (0, 3, expected.clone())
        );
        let reopened = State::open(store).await?.log("turns");
        assert_eq!((reopened.first(), reopened.entries()), (0, expected));
        Ok(())
    })
}

/// A truncation drops the entries before its index, whether it comes before
/// or after the appends of its mutation; those appended and dropped at once
/// are never stored.
#[test]
fn a_truncation_drops_a_prefix_the_mutations_appends_included() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let state = State::open(store.clone()).await?;
        let log = state.log("turns");
        let mut mutation = state.mutation();
        for entry in ["a", "b", "c"] {
            mutation.append(&log, entry.into())?;
        }
        mutation.flush().await?;
        let mut mutation = state.mutation();
        mutation.truncate(&log, 1)?;
        mutation.append(&log, "d".into())?;
        mutation.flush().await?;
        assert_eq!((log.first(), log.entries()), (1, entries(&["b", "c", "d"])));
        // Appended, then truncated past: the entry never lands. Each entry
        // is a blob of its own: one more, for "f".
        let mut mutation = state.mutation();
        mutation.append(&log, large(5))?;
        mutation.append(&log, "f".into())?;
        mutation.truncate(&log, 5)?;
        let blobs = store.blob_count();
        mutation.flush().await?;
        assert_eq!(
            store.blob_count(),
            blobs + 1,
            "the large entry, never stored"
        );
        assert_eq!(
            (log.first(), log.next(), log.entries()),
            (5, 6, entries(&["f"]))
        );
        // Truncated to its end: empty, and the next index goes on.
        let mut mutation = state.mutation();
        mutation.truncate(&log, 6)?;
        mutation.flush().await?;
        assert_eq!((log.first(), log.next(), log.entries()), (6, 6, vec![]));
        let reopened = State::open(store).await?.log("turns");
        assert_eq!((reopened.first(), reopened.next()), (6, 6));
        Ok(())
    })
}

/// A truncated entry's blob is garbage: a sweep marks it, and one
/// [`StateOptions::garbage_after`] versions later deletes it.
#[test]
fn a_truncated_entrys_blob_is_swept_later() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let options = StateOptions {
            garbage_after: 2,
            ..Default::default()
        };
        let state = State::open(store.clone()).options(options).await?;
        let log = state.log("turns");
        let mut mutation = state.mutation();
        mutation.append(&log, large(1))?;
        mutation.append(&log, large(2))?;
        mutation.flush().await?;
        assert_eq!(store.blob_count(), 2);
        let mut mutation = state.mutation();
        mutation.truncate(&log, 1)?;
        mutation.flush().await?;
        let swept = state.collect_garbage().await?;
        assert_eq!((swept.dead, swept.deleted), (1, 0), "marked: {swept:?}");
        for n in 0..2 {
            assert_eq!(store.blob_count(), 2, "a reader may still read it");
            let mut mutation = state.mutation();
            mutation.set(&state.record("n"), Bytes::from(vec![n]))?;
            mutation.flush().await?;
            state.collect_garbage().await?;
        }
        assert_eq!(store.blob_count(), 1);
        assert_eq!(log.entries(), vec![large(2)]);
        Ok(())
    })
}

/// A write refuses what no flush could store, before the mutation changes:
/// an entry too large, a log too long, a truncation past the end, or a name
/// of another kind.
#[test]
fn a_write_refuses_what_no_flush_could_store() -> Result<()> {
    futures::executor::block_on(async {
        let state = State::open(Arc::new(MemoryKv::default())).await?;
        let log = state.log("turns");
        let mut mutation = state.mutation();
        let error = mutation.append(&log, vec![0; MAX_RECORD_BYTES + 1].into());
        assert!(error.unwrap_err().to_string().contains("exceeds"));
        assert!(mutation.truncate(&log, 1).is_err(), "past its end");
        mutation.truncate(&log, 0)?;
        assert!(
            mutation.is_empty(),
            "refused, or dropping nothing: no change"
        );
        mutation.append(&log, "a".into())?;
        mutation.truncate(&log, 1)?;
        mutation.set(&state.record("progress"), "1".into())?;
        let error = mutation
            .append(&state.log("progress"), "x".into())
            .unwrap_err();
        assert!(
            error.to_string().contains("is a record, not a log"),
            "{error}"
        );
        let error = mutation
            .set(&state.record("turns"), "x".into())
            .unwrap_err();
        assert!(
            error.to_string().contains("is a log, not a record"),
            "{error}"
        );
        let error = mutation.write(&state.file("turns")?, 0, None).unwrap_err();
        assert!(
            error.to_string().contains("is a log, not a paged file"),
            "{error}"
        );
        mutation.flush().await?;
        // Full: one more is refused, until a truncation makes room.
        let mut mutation = state.mutation();
        for _ in 0..MAX_LOG_ENTRIES {
            mutation.append(&log, Bytes::new())?;
        }
        assert!(mutation.append(&log, Bytes::new()).is_err());
        mutation.truncate(&log, 2)?;
        mutation.append(&log, Bytes::new())?;
        mutation.flush().await?;
        assert_eq!((log.first(), log.next()), (2, 2 + MAX_LOG_ENTRIES));
        // Once flushed, a name keeps its kind.
        let mut mutation = state.mutation();
        assert!(mutation.set(&state.record("turns"), "x".into()).is_err());
        assert!(mutation.append(&state.log("progress"), "x".into()).is_err());
        assert!(mutation.is_empty());
        Ok(())
    })
}

/// A reader reads the log as it opened it while the owner appends and
/// truncates, its blobs included.
#[test]
fn a_reader_keeps_the_log_it_opened() -> Result<()> {
    futures::executor::block_on(async {
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let owner = State::open(store.clone()).await?;
        let log = owner.log("turns");
        let mut mutation = owner.mutation();
        mutation.append(&log, large(1))?;
        mutation.append(&log, "b".into())?;
        mutation.flush().await?;
        let reader = State::open(store.clone()).reader().await?;
        let mut mutation = owner.mutation();
        mutation.truncate(&log, 2)?;
        mutation.append(&log, "c".into())?;
        mutation.flush().await?;
        assert_eq!(reader.log("turns").entries(), vec![large(1), "b".into()]);
        assert_eq!((log.first(), log.entries()), (2, entries(&["c"])));
        let reopened = State::open(store).reader().await?;
        assert_eq!(reopened.log("turns").entries(), entries(&["c"]));
        Ok(())
    })
}

/// A mutation kept while another flush appends to its log: its flush is
/// refused, since the indexes its appends returned no longer hold.
#[test]
fn a_flush_refuses_appends_to_a_log_appended_to_since() -> Result<()> {
    futures::executor::block_on(async {
        let state = State::open(Arc::new(MemoryKv::default())).await?;
        let log = state.log("turns");
        let mut kept = state.mutation();
        assert_eq!(kept.append(&log, "kept".into())?, 0);
        let mut other = state.mutation();
        other.append(&log, "other".into())?;
        other.flush().await?;
        let error = kept.flush().await.unwrap_err();
        assert!(error.to_string().contains("appended to since"), "{error}");
        assert!(state.is_usable());
        assert_eq!(log.entries(), entries(&["other"]));
        Ok(())
    })
}
