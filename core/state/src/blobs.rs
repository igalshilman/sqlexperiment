//! The root in one store, the blobs in another: Restate's state keeps the
//! root, the commit point, which Restate fences, and the claim; an object
//! store such as S3 keeps the blobs (and the sweep's list of dead ones).
//!
//! [`SplitStore`] is a [`KvStore`] over the two. It queues every write in
//! memory, and its sync writes them in order, each write to the root's
//! store a barrier: the blobs issued before it, in parallel, then it; then a
//! sync of the root's store. A crash may keep any of the blobs, but never a
//! root without the blobs it names: what the state needs from the order.
//!
//! Its blobs are not fenced ([`KvStore::unfenced_blobs`]): an earlier
//! attempt that Restate has fenced from the root may still be putting them.
//! So the state claims its blob numbers in Restate, and syncs, before it
//! puts any: no two attempts ever put the same key.
//!
//! ```rust,ignore
//! let blobs = ObjectStoreBlobs::new(s3, "objects/42"); // the feature `object-store`
//! let state = State::open(Arc::new(SplitStore::new(restate, Arc::new(blobs)))).await?;
//! ```
//!
//! What no root names any more, a sweep deletes
//! ([`crate::State::collect_garbage`]): any process that reads Restate's
//! root and lists the blob store can run it, beside the owner.
use crate::kv::KvStore;
use crate::store::{CLAIM_KEY, ROOT_KEY};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, future::BoxFuture, lock::Mutex as AsyncMutex, stream};
use std::{
    io,
    sync::{Arc, Mutex},
};

/// Requests a sync keeps in flight to the blob store.
const PARALLEL: usize = 32;

/// A store of blobs, each durable once its request returns: S3, say,
/// through the feature `object-store` ([`ObjectStoreBlobs`]).
pub trait BlobStore: Send + Sync {
    /// The value of `key`; `None` if absent.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>>;
    /// Store `value` at `key`.
    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>>;
    /// Remove `key`; a missing key succeeds.
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>>;
    /// Every key under `prefix`.
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, io::Result<Vec<String>>>;
}

/// A [`KvStore`] that keeps the root and the claim in `root`, and the rest
/// in `blobs`: see the module documentation.
pub struct SplitStore {
    root: Arc<dyn KvStore>,
    blobs: Arc<dyn BlobStore>,
    /// The writes issued and not yet synced, in order; reads see them.
    queued: Mutex<Vec<(String, Option<Bytes>)>>,
    /// One sync at a time.
    syncing: AsyncMutex<()>,
}

/// Whether `key` lives in the root's store.
fn in_root(key: &str) -> bool {
    key == ROOT_KEY || key == CLAIM_KEY
}

impl SplitStore {
    pub fn new(root: Arc<dyn KvStore>, blobs: Arc<dyn BlobStore>) -> Self {
        Self {
            root,
            blobs,
            queued: Mutex::default(),
            syncing: AsyncMutex::new(()),
        }
    }

    fn queue(&self, key: &str, value: Option<Bytes>) -> io::Result<()> {
        self.queued.lock().unwrap().push((key.to_owned(), value));
        Ok(())
    }

    /// Write `blobs` in parallel, each durable when this returns.
    async fn write_blobs(&self, blobs: &[(String, Option<Bytes>)]) -> io::Result<()> {
        let mut writes: Vec<BoxFuture<'_, io::Result<()>>> = Vec::with_capacity(blobs.len());
        for (key, value) in blobs {
            writes.push(match value {
                Some(value) => self.blobs.put(key, value.clone()),
                None => self.blobs.delete(key),
            });
        }
        stream::iter(writes)
            .buffer_unordered(PARALLEL)
            .try_collect()
            .await
    }
}

impl KvStore for SplitStore {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async move {
            let queued = (self.queued.lock().unwrap().iter().rev())
                .find(|(queued, _)| queued == key)
                .map(|(_, value)| value.clone());
            match queued {
                Some(value) => Ok(value),
                None if in_root(key) => self.root.get(key).await,
                None => self.blobs.get(key).await,
            }
        })
    }

    fn put(&self, key: &str, value: Bytes) -> io::Result<()> {
        self.queue(key, Some(value))
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        self.queue(key, None)
    }

    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            let _syncing = self.syncing.lock().await;
            let writes = self.queued.lock().unwrap().clone();
            let synced = async {
                let mut blobs = Vec::new();
                for (key, value) in &writes {
                    if !in_root(key) {
                        blobs.push((key.clone(), value.clone()));
                        continue;
                    }
                    // A root goes only after the blobs issued before it.
                    self.write_blobs(&std::mem::take(&mut blobs)).await?;
                    match value {
                        Some(value) => self.root.put(key, value.clone())?,
                        None => self.root.delete(key)?,
                    }
                }
                self.write_blobs(&blobs).await?;
                self.root.sync().await
            }
            .await;
            // Synced or failed, they are no longer queued: what failed has an
            // unknown outcome, which the next read learns from the stores.
            self.queued.lock().unwrap().drain(..writes.len());
            synced
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, io::Result<Vec<String>>> {
        Box::pin(async move {
            let mut keys = self.blobs.list(prefix).await?;
            for (key, value) in self.queued.lock().unwrap().iter() {
                if key.starts_with(prefix) && !in_root(key) {
                    match value {
                        Some(_) => keys.push(key.clone()),
                        None => keys.retain(|listed| listed != key),
                    }
                }
            }
            keys.sort();
            keys.dedup();
            Ok(keys)
        })
    }

    fn unfenced_blobs(&self) -> bool {
        true
    }
}

#[cfg(feature = "object-store")]
pub use object::ObjectStoreBlobs;

#[cfg(feature = "object-store")]
mod object {
    use super::BlobStore;
    use bytes::Bytes;
    use futures::{TryStreamExt, future::BoxFuture};
    use object_store::{Error, ObjectStore, ObjectStoreExt, path::Path};
    use std::{io, sync::Arc};

    /// A [`BlobStore`] in an object store, under a prefix of its own (one per
    /// namespace: the object's key, say): S3 through `object_store`'s
    /// feature `aws`.
    pub struct ObjectStoreBlobs {
        store: Arc<dyn ObjectStore>,
        prefix: String,
    }

    impl ObjectStoreBlobs {
        /// The blobs of `store` under `prefix`: `objects/42`, say.
        pub fn new(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Self {
            Self {
                store,
                prefix: prefix.into(),
            }
        }

        fn path(&self, key: &str) -> Path {
            Path::from(format!("{}/{key}", self.prefix))
        }
    }

    impl BlobStore for ObjectStoreBlobs {
        fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
            Box::pin(async move {
                match self.store.get(&self.path(key)).await {
                    Ok(found) => Ok(Some(found.bytes().await?)),
                    Err(Error::NotFound { .. }) => Ok(None),
                    Err(error) => Err(error.into()),
                }
            })
        }

        fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
            Box::pin(async move {
                self.store.put(&self.path(key), value.into()).await?;
                Ok(())
            })
        }

        fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
            Box::pin(async move {
                match self.store.delete(&self.path(key)).await {
                    Ok(()) | Err(Error::NotFound { .. }) => Ok(()),
                    Err(error) => Err(error.into()),
                }
            })
        }

        fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, io::Result<Vec<String>>> {
            Box::pin(async move {
                let ours = format!("{}/", self.prefix);
                let listed: Vec<_> = (self.store.list(Some(&self.path(prefix))))
                    .map_ok(|meta| meta.location.as_ref().to_owned())
                    .try_collect()
                    .await?;
                Ok(listed
                    .into_iter()
                    .filter_map(|path| Some(path.strip_prefix(&ours)?.to_owned()))
                    .filter(|key| key.starts_with(prefix))
                    .collect())
            })
        }
    }
}
