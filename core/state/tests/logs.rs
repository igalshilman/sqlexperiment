//! Logs: entries appended in a mutation are read once flushed, from the
//! index each took, and survive a reopen; a truncation drops a prefix, the
//! entries the same mutation appends included, and checkpoints drop them;
//! a write refuses what no flush could store; a reader keeps the log it
//! opened.
#![cfg(feature = "simulation")]
use bytes::Bytes;
use state::{
    MAX_LOG_ENTRIES, MAX_RECORD_BYTES, Result, State, StateOptions, kv::KvStore, simulation::*,
};
use std::{collections::BTreeSet, sync::Arc};

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
        // Appended, then truncated past: the entry never lands.
        let mut mutation = state.mutation();
        mutation.append(&log, large(5))?;
        mutation.append(&log, "f".into())?;
        mutation.truncate(&log, 5)?;
        let report = mutation.flush().await?;
        assert!(
            report.entry_bytes < 100,
            "the large entry, never stored: {report:?}"
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

/// An entry that does not compress: stored, it is as large as it is.
fn noise(n: u8, len: usize) -> Bytes {
    let mut seed = u64::from(n) + 1;
    (0..len)
        .map(|_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 56) as u8
        })
        .collect()
}

/// A truncated entry leaves the tail with the checkpoint after the next, and
/// is in no checkpoint since: the pack that held it, mostly dead, is
/// compacted into a later one, then deleted.
#[test]
fn a_truncated_entry_is_dropped_by_checkpoints() -> Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(MemoryKv::default());
        let options = StateOptions {
            checkpoint_entries: 1,
            ..Default::default()
        };
        let state = State::open(store.clone()).options(options).await?;
        let log = state.log("turns");
        let mut mutation = state.mutation();
        mutation.append(&log, noise(1, 60_000))?;
        mutation.append(&log, noise(2, 2_000))?;
        mutation.flush().await?;
        assert_eq!(store.tail_entries(), BTreeSet::from([1]));
        let mut mutation = state.mutation();
        mutation.truncate(&log, 1)?;
        let report = mutation.flush().await?;
        let first = report.checkpoint.expect("the first checkpoint");
        assert_eq!((first.version, first.values), (1, 2), "both live then");
        assert!(first.pack_bytes > 60_000, "{first:?}");
        let mut checkpoints = Vec::new();
        for n in 0..3 {
            let mut mutation = state.mutation();
            mutation.set(&state.record("n"), Bytes::from(vec![n]))?;
            checkpoints.push(mutation.flush().await?.checkpoint.expect("a checkpoint"));
        }
        assert_eq!(checkpoints[0].compacted, 1, "{checkpoints:?}");
        assert!(checkpoints[0].pack_bytes < 10_000, "{checkpoints:?}");
        assert_eq!(checkpoints[1].deleted, 1, "{checkpoints:?}");
        assert_eq!(store.tail_entries(), BTreeSet::from([4, 5]));
        assert!(store.stored_bytes() < 20_000, "{}", store.stored_bytes());
        let reopened = State::open(store).await?.log("turns");
        assert_eq!(reopened.entries(), vec![noise(2, 2_000)]);
        assert_eq!(log.entries(), vec![noise(2, 2_000)]);
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
