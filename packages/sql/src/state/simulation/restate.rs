//! Restate's state of one object, as the attempts of its handler see it:
//! each new attempt fences those before it, whose writes Restate refuses,
//! though they may still run (zombies).
use super::{Effect, MemoryKv};
use crate::state::kv::KvStore;
use bytes::Bytes;
use futures::future::BoxFuture;
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// One object's state in Restate; clones share it.
#[derive(Clone, Default)]
pub struct Restate {
    /// What Restate stored.
    pub store: MemoryKv,
    attempts: Arc<AtomicU64>,
    /// How the next sync of an attempt fails, if it does.
    sync_fault: Arc<Mutex<Option<Effect>>>,
}

impl Restate {
    /// A new attempt's view, which fences every earlier one.
    pub fn attempt(&self) -> Arc<dyn KvStore> {
        let epoch = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        Arc::new(Attempt {
            restate: self.clone(),
            epoch,
            queued: Mutex::default(),
        })
    }

    /// A view that reads beside the attempts and fences none: a worker's,
    /// another invocation's. It writes nothing.
    pub fn reader(&self) -> Arc<dyn KvStore> {
        Arc::new(Reader(self.store.clone()))
    }

    /// The next sync of an attempt fails: before it stores anything
    /// ([`Effect::None`]), or after ([`Effect::After`]: the acknowledgement
    /// is lost); `None` heals it.
    pub fn fail_next_sync(&self, effect: Option<Effect>) {
        *self.sync_fault.lock().unwrap() = effect;
    }
}

/// An attempt's view of Restate: its writes wait for its sync, which fails
/// once a later attempt fenced it.
struct Attempt {
    restate: Restate,
    epoch: u64,
    queued: Mutex<Vec<(String, Option<Bytes>)>>,
}

impl Attempt {
    fn fenced(&self) -> io::Result<()> {
        match self.restate.attempts.load(Ordering::SeqCst) == self.epoch {
            true => Ok(()),
            false => Err(io::Error::other("fenced: a later attempt runs")),
        }
    }
}

impl KvStore for Attempt {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async move {
            let queued = (self.queued.lock().unwrap().iter().rev())
                .find(|(queued, _)| queued == key)
                .map(|(_, value)| value.clone());
            match queued {
                Some(value) => Ok(value),
                None => self.restate.store.get(key).await,
            }
        })
    }
    fn put(&self, key: &str, value: Bytes) -> io::Result<()> {
        self.fenced()?;
        self.queued.lock().unwrap().push((key.into(), Some(value)));
        Ok(())
    }
    fn delete(&self, key: &str) -> io::Result<()> {
        self.fenced()?;
        self.queued.lock().unwrap().push((key.into(), None));
        Ok(())
    }
    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            let queued = std::mem::take(&mut *self.queued.lock().unwrap());
            self.fenced()?;
            let fault = self.restate.sync_fault.lock().unwrap().take();
            if let Some(Effect::None) = fault {
                return Err(io::Error::other("injected Restate sync failure"));
            }
            for (key, value) in queued {
                match value {
                    Some(value) => self.restate.store.put(&key, value)?,
                    None => self.restate.store.delete(&key)?,
                }
            }
            self.restate.store.sync().await?;
            match fault {
                Some(_) => Err(io::Error::other("injected Restate sync failure, after")),
                None => Ok(()),
            }
        })
    }
    fn keys(&self) -> BoxFuture<'_, io::Result<Vec<String>>> {
        KvStore::keys(&self.restate.store)
    }
}

/// A view that only reads.
struct Reader(MemoryKv);

impl KvStore for Reader {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        self.0.get(key)
    }
    fn put(&self, key: &str, _: Bytes) -> io::Result<()> {
        Err(io::Error::other(format!("a reader of Restate wrote {key}")))
    }
    fn delete(&self, key: &str) -> io::Result<()> {
        Err(io::Error::other(format!(
            "a reader of Restate deleted {key}"
        )))
    }
    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn keys(&self) -> BoxFuture<'_, io::Result<Vec<String>>> {
        KvStore::keys(&self.0)
    }
}
