//! Futures dropped after every number of polls: a commit cancelled midway
//! ends its volume and lands wholly or not at all, and the volume reopens
//! and goes on; one never polled did nothing; a cancelled open leaves the
//! owner's cache usable.
#![cfg(feature = "simulation")]
mod support;

use futures::task::noop_waker_ref;
use std::{
    pin::pin,
    sync::Arc,
    task::{Context, Poll},
};
use support::*;
use volume::{NamespaceCache, Result, StorageOptions, kv::KvStore, simulation::*};

/// Every commit checkpoints, and large transactions go to stored values.
fn options(cache: Option<NamespaceCache>) -> StorageOptions {
    StorageOptions {
        cache,
        checkpoint_after: 0,
        inline_log_bytes: 200,
        ..StorageOptions::default()
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

/// A store after a few commits, and what it holds.
async fn prepared() -> Result<(MemoryKv, Model)> {
    let memory = MemoryKv::default();
    let mut model = Model::default();
    let mut rng = Rng::new(0xca9ce1);
    for _ in 0..4 {
        let session = Session::open(Arc::new(memory.clone()), options(None)).await?;
        let ops: Vec<Op> = (0..8).map(|_| Op::random(&mut rng)).collect();
        for op in &ops {
            session.apply(op)?;
        }
        session.volume.commit().await?;
        model = model.applied(&ops);
    }
    Ok((memory, model))
}

#[test]
fn a_commit_cancelled_after_any_number_of_polls_lands_wholly_or_not_at_all() -> Result<()> {
    futures::executor::block_on(async {
        let (prepared, old) = prepared().await?;
        let mut rng = Rng::new(0xd20);
        let ops: Vec<Op> = (0..12).map(|_| Op::random(&mut rng)).collect();
        let new = old.applied(&ops);
        // Reopened by the owner, or after a crash.
        for (cached, crash) in [(false, false), (false, true), (true, false), (true, true)] {
            let mut landed = [0; 2];
            for polls in 0.. {
                // Slow requests: a commit takes several polls.
                let memory = prepared.fork();
                let fault = FaultKv::new(memory.clone(), 5);
                let store = Arc::new(MonitorKv::new(fault).with_base("vm", golden()));
                let cache = cached.then(fresh_namespace);
                let session = Session::open(store.clone(), options(cache.clone())).await?;
                for op in &ops {
                    session.apply(op)?;
                }
                let volume = &session.volume;
                match poll_times(volume.commit(), polls) {
                    Some(outcome) => {
                        outcome?;
                        assert!(polls > 2, "a commit takes several polls");
                        break;
                    }
                    // Never polled: it did nothing, and the changes wait.
                    None if polls == 0 => {
                        assert!(volume.is_usable());
                        assert_eq!(memory.unsynced(), vec![]);
                        volume.commit().await?;
                        assert_eq!(new.differs(&session).await?, None);
                        continue;
                    }
                    None => {}
                }
                let case = format!("cached {cached}, crash {crash}, dropped after {polls} polls");
                assert!(!volume.is_usable(), "{case}");
                let error = volume.commit().await.unwrap_err().to_string();
                assert!(error.contains("ended"), "{case}: {error}");
                drop(session);
                if crash {
                    memory.crash();
                }
                let reopened = Session::open(store.clone(), options(cache)).await?;
                let whole = new.matches(&reopened).await?;
                assert!(whole || old.matches(&reopened).await?, "{case}");
                landed[usize::from(whole)] += 1;
                // It goes on, durably.
                let after = Op::Set("state", "after".into());
                reopened.apply(&after)?;
                reopened.volume.commit().await?;
                drop(reopened);
                memory.crash();
                let reopened = Session::open(store.clone(), options(None)).await?;
                let expected = if whole { &new } else { &old };
                assert_eq!(
                    expected.applied(&[after]).differs(&reopened).await?,
                    None,
                    "{case}"
                );
                assert_eq!(store.violations(), Vec::<String>::new(), "{case}");
            }
            // Without a crash the owner reads its writes; after one they
            // are gone, as no sync completed.
            let expected = if crash {
                [landed[0], 0]
            } else {
                [0, landed[1]]
            };
            assert_eq!(landed, expected, "cached {cached}, crash {crash}");
        }
        Ok(())
    })
}

/// An owner's open dropped midway, at each poll: the next open reads the
/// state, and commits.
#[test]
fn an_open_cancelled_at_any_poll_leaves_the_cache_usable() -> Result<()> {
    futures::executor::block_on(async {
        let (memory, mut model) = prepared().await?;
        let fault = FaultKv::new(memory, 3);
        let store: Arc<dyn KvStore> = Arc::new(MonitorKv::new(fault).with_base("vm", golden()));
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
            let op = Op::random(&mut rng);
            session.apply(&op)?;
            session.volume.commit().await?;
            model.apply(&op);
        }
        Ok(())
    })
}
