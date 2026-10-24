//! The root in one store, the blobs in another: Restate's state keeps the
//! root, the commit point, which Restate fences; an object store such as S3
//! keeps the blobs, written once each.
//!
//! [`SplitStore`] is a [`KvStore`] over the two. It queues every write in
//! memory, and its sync writes them in order with the root as a barrier: the
//! blobs issued before a root, in parallel, then that root, then a sync of
//! the root's store. A crash may keep any of the blobs, but never a root
//! without the blobs it names, which is what the state needs from the
//! order. Its blobs are write-once ([`KvStore::write_once_blobs`]), so an
//! earlier attempt that Restate has fenced from the root, still writing
//! blobs, can only fail its own flush or the new attempt's, never replace a
//! blob a root names.
//!
//! ```rust,ignore
//! let blobs = ObjectStoreBlobs::new(s3, "objects/42"); // the feature `object-store`
//! let state = State::open(Arc::new(SplitStore::new(restate, Arc::new(blobs)))).await?;
//! ```
//!
//! A flush whose outcome is unknown may leave blobs no root names, which no
//! later flush overwrites or deletes: sweep them, past a grace period, with
//! [`orphans`].
use crate::kv::KvStore;
use crate::map::{Node, Nodes, Visit};
use crate::store::{BLOB_PREFIX, Kind, ROOT_KEY, Root, Value, blob_key, invalid, parse_blob_key};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, future::BoxFuture, lock::Mutex as AsyncMutex, stream};
use std::{
    collections::{BTreeSet, HashMap},
    io,
    sync::{Arc, Mutex},
};

/// Requests a sync keeps in flight to the blob store.
const PARALLEL: usize = 32;

/// A store of write-once blobs, each durable once its request returns: S3,
/// say, through the feature `object-store` ([`ObjectStoreBlobs`]).
pub trait BlobStore: Send + Sync {
    /// The value of `key`; `None` if absent.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>>;
    /// Store `value` at `key`, unless the key holds a value: then fail with
    /// [`io::ErrorKind::AlreadyExists`] (S3's `If-None-Match: *`).
    fn put_new<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>>;
    /// Remove `key`; a missing key succeeds.
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>>;
    /// Every key under `prefix`.
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, io::Result<Vec<String>>>;
}

/// A [`KvStore`] that keeps the root in `root` and the blobs in `blobs`:
/// see the module documentation.
pub struct SplitStore {
    root: Arc<dyn KvStore>,
    blobs: Arc<dyn BlobStore>,
    /// The writes issued and not yet synced, in order; reads see them.
    queued: Mutex<Vec<(String, Option<Bytes>)>>,
    /// One sync at a time.
    syncing: AsyncMutex<()>,
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
                Some(value) => Box::pin(self.put_blob(key, value.clone())),
                None => self.blobs.delete(key),
            });
        }
        stream::iter(writes)
            .buffer_unordered(PARALLEL)
            .try_collect()
            .await
    }

    /// Put a blob once. A key that holds the same bytes is this writer's own
    /// put, retried; other bytes are another writer's, which fails the sync.
    async fn put_blob(&self, key: &str, value: Bytes) -> io::Result<()> {
        match self.blobs.put_new(key, value.clone()).await {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                match self.blobs.get(key).await? {
                    Some(held) if held == value => Ok(()),
                    _ => Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{key} holds another writer's blob; reopen"),
                    )),
                }
            }
            result => result,
        }
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
                None if key == ROOT_KEY => self.root.get(key).await,
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
                    if key != ROOT_KEY {
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

    fn write_once_blobs(&self) -> bool {
        true
    }
}

/// The blobs (keys under `v16/blob/`) in `blobs` that the root `root` (as
/// stored) neither names nor holds as garbage: those of flushes whose
/// outcome was unknown, or of earlier attempts fenced from the root. Delete
/// them only past a grace period: a flush running now puts blobs that no
/// stored root names yet.
pub async fn orphans(root: &[u8], blobs: &dyn BlobStore) -> io::Result<Vec<String>> {
    let root = Root::decode(root)?;
    let mut known = named(&root, blobs).await?;
    known.extend(root.garbage.keys());
    let listed = blobs.list(BLOB_PREFIX).await?;
    Ok(listed
        .into_iter()
        .filter(|key| parse_blob_key(key).is_some_and(|n| !known.contains(&n)))
        .collect())
}

/// Every blob `root` names, walking each file's map through its node blobs.
async fn named(root: &Root, blobs: &dyn BlobStore) -> io::Result<BTreeSet<u64>> {
    let mut named = BTreeSet::new();
    for entry in root.directory.values() {
        match &entry.kind {
            Kind::Record(Value::Blob(n)) => {
                named.insert(*n);
            }
            Kind::Record(Value::Inline(_)) => {}
            Kind::Log { entries, .. } => named.extend(entries),
            Kind::Paged { size, map, .. } => {
                let mut decoded: HashMap<u64, Arc<Node>> = HashMap::new();
                loop {
                    let mut met = Vec::new();
                    let missing = {
                        let get = |n: u64| decoded.get(&n).cloned();
                        let nodes = Nodes::new(&get);
                        map.walk(root.shape(*size), &nodes, &mut |visit| met.push(visit));
                        nodes.missing()
                    };
                    if missing.is_empty() {
                        for visit in met {
                            match visit {
                                Visit::Node(n) => {
                                    named.insert(n);
                                }
                                Visit::Chunk(_, chunk) => named.extend(chunk.blobs()),
                            }
                        }
                        break;
                    }
                    for need in missing {
                        let blob = need.blob;
                        let frame = (blobs.get(&blob_key(blob)).await?)
                            .ok_or_else(|| invalid("a map node the root names"))?;
                        decoded.insert(blob, Arc::new(Node::from_frame(&frame, need)?));
                    }
                }
            }
        }
    }
    Ok(named)
}

#[cfg(feature = "object-store")]
pub use object::ObjectStoreBlobs;

#[cfg(feature = "object-store")]
mod object {
    use super::BlobStore;
    use bytes::Bytes;
    use futures::{TryStreamExt, future::BoxFuture};
    use object_store::{Error, ObjectStore, ObjectStoreExt, PutMode, PutOptions, path::Path};
    use std::{io, sync::Arc};

    /// A [`BlobStore`] in an object store, under a prefix of its own (one per
    /// namespace): S3 through `object_store`'s feature `aws`, say, whose
    /// conditional puts (`If-None-Match: *`) keep each blob written once.
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

        fn put_new<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
            Box::pin(async move {
                let create = PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                };
                match self
                    .store
                    .put_opts(&self.path(key), value.into(), create)
                    .await
                {
                    Ok(_) => Ok(()),
                    Err(error @ Error::AlreadyExists { .. }) => {
                        Err(io::Error::new(io::ErrorKind::AlreadyExists, error))
                    }
                    Err(error) => Err(error.into()),
                }
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
