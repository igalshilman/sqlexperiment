//! A crash at every write of every flush of a sequence that sets records
//! (inline and in blobs), writes several files (one with a base), shrinks
//! one and rebases chunks: each flush is atomic, durable once its root is,
//! and the state goes on; where blobs are not fenced, its claim too. And a
//! crash at every write of a sweep: it never deletes what a root names, and
//! the next sweeps finish its work.
#![cfg(feature = "simulation")]
mod support;

use state::{Base, Collected, ROOT_KEY, Result, StateOptions, kv::KvStore, simulation::*};
use std::sync::Arc;
use support::*;

/// Flushes in the sequence.
const STEPS: u64 = 10;

fn options(cache: Option<Namespace>) -> Setup {
    Setup {
        options: StateOptions {
            chunk_pages: 2,
            fanout: 2,
            rebase_ratio: 0.5,
            inline_record_bytes: 150,
            prefetch_pages: 0,
            garbage_after: 1,
            // The flush's sync fails first; sweeps crash in the campaign.
            sweep_every: 0,
        },
        cache,
    }
}

/// The changes of flush `step`: small ones (deltas), large ones (rebases),
/// records over the inline budget, holes, a shrink, and a page equal to its
/// base's.
fn changes(step: u64) -> Vec<Op> {
    let mut rng = Rng::new(step);
    let image = |rng: &mut Rng| match step % 3 {
        0 => varied(rng),
        _ => page(step),
    };
    let mut ops = vec![
        Op::Set("progress", format!("step {step}").into_bytes().into()),
        Op::Write("db", step % 4, Some(image(&mut rng))),
        Op::Write("aux", 4 + step % 3, Some(image(&mut rng))),
        Op::Write("vm", step % 6, Some(image(&mut rng))),
    ];
    if step % 2 == 1 {
        ops.push(Op::Set("outbox", rng.bytes(600).into()));
    }
    if step % 3 == 2 {
        ops.push(Op::SetLen("aux", 512 * 9));
        let base = golden().page(1).map(state::Page::copy_from_slice);
        ops.push(Op::Write("vm", 1, base));
    }
    if step % 4 == 3 {
        ops.push(Op::Write("db", 1, None));
    }
    ops
}

/// Open a session and read a few pages, so that a flush finds their chunks
/// decoded.
async fn session(store: Arc<dyn KvStore>, cache: Option<Namespace>) -> Result<Session> {
    let session = Session::open(store, options(cache)).await?;
    for name in FILES {
        session.file(name).read(0).await?;
    }
    Ok(session)
}

#[test]
fn a_flush_is_atomic_at_every_write_of_a_sequence() -> Result<()> {
    let kinds = futures::executor::block_on(crash_every_flush(MemoryKv::default()))?;
    assert_eq!(kinds, ["blob put", "root put"]);
    Ok(())
}

/// Where blobs are not fenced, a flush first claims its numbers, and syncs:
/// a crash there loses the flush, and the claim is only ever numbered past.
/// (The store panics at a blob key put twice.)
#[test]
fn a_flush_that_claims_is_atomic_at_every_write() -> Result<()> {
    let kinds = futures::executor::block_on(crash_every_flush(MemoryKv::unfenced()))?;
    assert_eq!(kinds, ["blob put", "claim put", "root put"]);
    Ok(())
}

/// The syncs `run` makes on a copy of `durable`, once it opened a session.
async fn syncs<F: AsyncFnOnce(&Session) -> Result<()>>(
    durable: &MemoryKv,
    run: F,
) -> Result<usize> {
    let dry = durable.fork();
    let opened = session(Arc::new(dry.clone()), None).await?;
    dry.reset_stats();
    run(&opened).await?;
    Ok(dry.stats().syncs as usize)
}

/// Fail at its every sync each flush of the sequence on `durable`, then
/// crash keeping every prefix of the writes queued: the kinds of writes it
/// crashed at.
async fn crash_every_flush(durable: MemoryKv) -> Result<Vec<String>> {
    let mut model = Model::default();
    let (mut cases, mut kinds) = (0, std::collections::BTreeSet::new());
    for step in 0..STEPS {
        let ops = changes(step);
        let new = model.applied(&ops);
        let syncs = syncs(&durable, async |session| {
            session.flush(&ops).await.map(drop)
        })
        .await?;
        for (cached, nth) in [false, true]
            .into_iter()
            .flat_map(|c| (1..=syncs).map(move |n| (c, n)))
        {
            // The flush fails at its `nth` sync, the writes since the
            // last one queued.
            let fault = Arc::new(FaultKv::new(durable.fork(), 0));
            let cache = cached.then(fresh_namespace);
            let failing = session(fault.clone(), cache.clone()).await?;
            let _signal = fault.arm(Failpoint {
                operation: Operation::Sync,
                key_prefix: String::new(),
                nth,
                effect: Effect::None,
                cancel: false,
            });
            assert!(failing.flush(&ops).await.is_err());
            fault.disarm();
            drop(failing);
            let writes = fault.inner.unsynced();
            // A claim's sync precedes the root: a crash there loses it.
            let record =
                (writes.iter().position(|(key, _)| key == ROOT_KEY)).unwrap_or(writes.len());
            for (key, value) in &writes {
                let kind = object_kind(key);
                kinds.insert(format!(
                    "{kind} {}",
                    if value.is_some() { "put" } else { "delete" }
                ));
            }
            for kept in 0..=writes.len() {
                cases += 1;
                let memory = fault.inner.fork();
                memory.crash_keeping(kept);
                let store = Arc::new(
                    MonitorKv::seeded(memory.clone(), memory.values()).with_base("vm", golden()),
                );
                // The owner's cache saw the failure; reading, it follows
                // this crash's history as well as any.
                let reopened = Session::open(store.clone(), options(cache.clone())).await?;
                let expected = if kept > record { &new } else { &model };
                let case = format!(
                    "step {step}, cached {cached}, sync {nth}: {kept} of {} writes",
                    writes.len()
                );
                assert_eq!(expected.differs(&reopened).await?, None, "{case}");
                drop(reopened);
                // A next flush, by a new owner, survives a crash.
                let next = [
                    Op::Set("state", "next".into()),
                    Op::Write("db", 9, Some(page(99))),
                ];
                let after = session(store.clone(), cached.then(fresh_namespace)).await?;
                after.flush(&next).await?;
                drop(after);
                memory.crash();
                let reopened = Session::open(store.clone(), options(None)).await?;
                let next = expected.applied(&next);
                assert_eq!(
                    next.differs(&reopened).await?,
                    None,
                    "{case}, then another flush"
                );
                assert_eq!(store.violations(), Vec::<String>::new(), "{case}");
            }
        }
        // The sequence goes on from the flush acknowledged.
        let store: Arc<dyn KvStore> = Arc::new(durable.clone());
        session(store, None).await?.flush(&ops).await?;
        model = new;
    }
    println!("crashed flushes: {cases} cases");
    Ok(kinds.into_iter().collect())
}

#[test]
fn a_sweep_never_deletes_what_a_root_names_at_every_write() -> Result<()> {
    futures::executor::block_on(async {
        for durable in [MemoryKv::default(), MemoryKv::unfenced()] {
            let kinds = crash_every_sweep(durable).await?;
            assert_eq!(kinds, ["blob delete", "sweep put"]);
        }
        Ok(())
    })
}

/// Flush a few steps of the sequence on `durable`, sweep, flush another (so
/// that the next sweep deletes), then fail that sweep at its every sync, and
/// crash keeping every prefix of the writes queued.
async fn crash_every_sweep(durable: MemoryKv) -> Result<Vec<String>> {
    let open = |store: &MemoryKv| session(Arc::new(store.clone()), None);
    let mut model = Model::default();
    for step in 0..STEPS {
        if step == STEPS - 1 {
            open(&durable).await?.state.collect_garbage().await?;
        }
        let ops = changes(step);
        open(&durable).await?.flush(&ops).await?;
        model = model.applied(&ops);
    }
    let sweep = async |session: &Session| session.state.collect_garbage().await.map(drop);
    let syncs = syncs(&durable, sweep).await?;
    let (mut cases, mut kinds) = (0, std::collections::BTreeSet::new());
    for nth in 1..=syncs {
        let fault = Arc::new(FaultKv::new(durable.fork(), 0));
        let failing = session(fault.clone(), None).await?;
        let _signal = fault.arm(Failpoint {
            operation: Operation::Sync,
            key_prefix: String::new(),
            nth,
            effect: Effect::None,
            cancel: false,
        });
        assert!(failing.state.collect_garbage().await.is_err());
        fault.disarm();
        drop(failing);
        let writes = fault.inner.unsynced();
        for (key, value) in &writes {
            let kind = object_kind(key);
            kinds.insert(format!(
                "{kind} {}",
                if value.is_some() { "put" } else { "delete" }
            ));
        }
        for kept in 0..=writes.len() {
            cases += 1;
            let case = format!("sync {nth}: {kept} of {} writes", writes.len());
            let memory = fault.inner.fork();
            memory.crash_keeping(kept);
            let store = Arc::new(
                MonitorKv::seeded(memory.clone(), memory.values()).with_base("vm", golden()),
            );
            let reopened = Session::open(store.clone(), options(None)).await?;
            assert_eq!(model.differs(&reopened).await?, None, "{case}");
            // The next sweeps finish the work: only what the root names
            // is left.
            let mut swept: Vec<Collected> = Vec::new();
            for tick in 0..3 {
                swept.push(reopened.state.collect_garbage().await?);
                let ops = [Op::Set("progress", format!("tick {tick}").into())];
                reopened.flush(&ops).await?;
            }
            let last = &swept[2];
            assert!(
                !last.waiting && last.deleted == swept[1].dead,
                "{case}: {swept:#?}"
            );
            assert_eq!(
                last.listed - last.deleted - last.dead,
                last.named,
                "{case}: {swept:#?}"
            );
            assert_eq!(store.violations(), Vec::<String>::new(), "{case}");
        }
    }
    println!("crashed sweeps: {cases} cases");
    Ok(kinds.into_iter().collect())
}
