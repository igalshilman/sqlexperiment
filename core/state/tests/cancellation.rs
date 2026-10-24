//! Futures dropped after every number of polls: a flush cancelled midway
//! ends its state and lands wholly or not at all, and the state reopens
//! and goes on; one never polled did nothing, and its mutation went with it; a
//! cancelled open leaves the owner's cache usable.
#![cfg(feature = "simulation")]
mod support;

use futures::task::noop_waker_ref;
use state::{ROOT_KEY, Result, StateOptions, kv::KvStore, simulation::*};
use std::{
    pin::pin,
    sync::Arc,
    task::{Context, Poll},
};
use support::*;

/// Chunks rebase often, garbage goes at once, and large records go to blobs.
fn options(cache: Option<Namespace>) -> Setup {
    Setup {
        options: StateOptions {
            chunk_pages: 2,
            rebase_ratio: 0.2,
            garbage_after: 1,
            inline_record_bytes: 200,
            ..StateOptions::default()
        },
        cache,
    }
}

/// Poll `future` up to `polls` times: its output, if it completed.
fn poll_times<F: Future>(future: F, polls: usize) -> Option<F::Output> {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(noop_waker_ref());
    (0..polls).find_map(|_| match future.as_mut().poll(&mut cx) {
        Poll::Ready(output) => Some(output),
        Poll::Pending => None,
    })
}

/// A store after a few flushes, and what it holds.
async fn prepared() -> Result<(MemoryKv, Model)> {
    let memory = MemoryKv::default();
    let mut model = Model::default();
    let mut rng = Rng::new(0xca9ce1);
    for _ in 0..4 {
        let session = Session::open(Arc::new(memory.clone()), options(None)).await?;
        let ops: Vec<Op> = (0..8).map(|_| Op::random(&mut rng)).collect();
        session.flush(&ops).await?;
        model = model.applied(&ops);
    }
    Ok((memory, model))
}

#[test]
fn a_flush_cancelled_after_any_number_of_polls_lands_wholly_or_not_at_all() -> Result<()> {
    futures::executor::block_on(async {
        let (prepared, old) = prepared().await?;
        let mut rng = Rng::new(0xd20);
        let ops: Vec<Op> = (0..12).map(|_| Op::random(&mut rng)).collect();
        let new = old.applied(&ops);
        // Reopened by the owner, or after a crash.
        for (cached, crash) in [(false, false), (false, true), (true, false), (true, true)] {
            let mut landed = [0; 2];
            for polls in 0.. {
                // Slow requests: a flush takes several polls.
                let memory = prepared.fork();
                let fault = FaultKv::new(memory.clone(), 5);
                let store =
                    Arc::new(MonitorKv::seeded(fault, memory.values()).with_base("vm", golden()));
                let cache = cached.then(fresh_namespace);
                let session = Session::open(store.clone(), options(cache.clone())).await?;
                if cached {
                    // Read in full, so the flush fetches nothing: a miss
                    // waits on the cache's disk threads, not on polls.
                    assert_eq!(old.differs(&session).await?, None);
                }
                let mut mutation = session.state.mutation();
                for op in &ops {
                    session.apply(&mut mutation, op)?;
                }
                let receipt = mutation.receipt();
                let state = &session.state;
                match poll_times(mutation.flush(), polls) {
                    Some(outcome) => {
                        outcome?;
                        assert!(polls > 2, "a flush takes several polls");
                        break;
                    }
                    // Never polled: nothing happened, and the mutation went with
                    // the future.
                    None if polls == 0 => {
                        assert!(state.is_usable());
                        assert!(receipt.is_discarded());
                        assert_eq!(memory.unsynced(), vec![]);
                        assert_eq!(old.differs(&session).await?, None);
                        continue;
                    }
                    None => {}
                }
                let case = format!("cached {cached}, crash {crash}, dropped after {polls} polls");
                assert!(!state.is_usable(), "{case}");
                assert!(
                    !receipt.is_flushed() && !receipt.is_discarded(),
                    "{case}: an unknown outcome settles the receipt neither way"
                );
                let error = state.mutation().flush().await.unwrap_err().to_string();
                assert!(error.contains("ended"), "{case}: {error}");
                drop(session);
                // The flush lands with its root; a crash takes it back, as
                // no sync completed.
                let rooted = memory.unsynced().iter().any(|(key, _)| key == ROOT_KEY);
                if crash {
                    memory.crash();
                }
                let reopened = Session::open(store.clone(), options(cache)).await?;
                let whole = new.matches(&reopened).await?;
                assert!(whole || old.matches(&reopened).await?, "{case}");
                assert_eq!(whole, rooted && !crash, "{case}");
                landed[usize::from(whole)] += 1;
                // It goes on, durably.
                let after = [Op::Set("state", "after".into())];
                reopened.flush(&after).await?;
                drop(reopened);
                memory.crash();
                let reopened = Session::open(store.clone(), options(None)).await?;
                let expected = if whole { &new } else { &old };
                assert_eq!(
                    expected.applied(&after).differs(&reopened).await?,
                    None,
                    "{case}"
                );
                assert_eq!(store.violations(), Vec::<String>::new(), "{case}");
            }
            // Some drops came after the root went out (and, uncached, some
            // while the flush still fetched what it builds on).
            if !crash {
                assert!(
                    landed[1] > 0 && (cached || landed[0] > 0),
                    "cached {cached}: {landed:?}"
                );
            }
        }
        Ok(())
    })
}

/// An owner's open dropped midway, at each poll: the next open reads the
/// state, and flushes.
#[test]
fn an_open_cancelled_at_any_poll_leaves_the_cache_usable() -> Result<()> {
    futures::executor::block_on(async {
        let (memory, mut model) = prepared().await?;
        let seed = memory.values();
        let fault = FaultKv::new(memory, 3);
        let store: Arc<dyn KvStore> =
            Arc::new(MonitorKv::seeded(fault, seed).with_base("vm", golden()));
        let cache = fresh_namespace();
        let mut rng = Rng::new(0x09e2);
        for polls in 0.. {
            let open = Session::open(store.clone(), options(Some(cache.clone())));
            if poll_times(open, polls).is_some() {
                assert!(polls > 2, "an open takes several polls");
                break;
            }
            let session = Session::open(store.clone(), options(Some(cache.clone()))).await?;
            assert_eq!(model.differs(&session).await?, None, "after {polls} polls");
            let ops = [Op::random(&mut rng)];
            session.flush(&ops).await?;
            model = model.applied(&ops);
        }
        Ok(())
    })
}
