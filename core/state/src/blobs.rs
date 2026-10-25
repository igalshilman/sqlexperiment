//! Where checkpoints' packs are: a [`BlobStore`], S3 through
//! [`ObjectStoreBlobs`] (the feature `object-store`), or Restate's state
//! itself ([`KvBlobs`], the default).
//!
//! ```rust,ignore
//! let packs = ObjectStoreBlobs::new(s3, "objects/42"); // the feature `object-store`
//! let state = State::open(restate).checkpoints(Arc::new(packs)).await?;
//! ```
//!
//! A pack is put once, under a name no other put uses, read in ranges, and
//! deleted once no checkpoint needs it ([`crate::State::collect_garbage`]):
//! a store needs no conditional put, and no fencing.
use crate::kv::KvStore;
use bytes::{Bytes, BytesMut};
use futures::future::{BoxFuture, try_join_all};
use std::{collections::BTreeSet, io, ops::Range, sync::Arc};

/// A store of blobs: S3, say, through the feature `object-store`
/// ([`ObjectStoreBlobs`]). A blob is durable once its put returns, or, in
/// [`KvBlobs`], with the next sync of the head that names it.
pub trait BlobStore: Send + Sync {
    /// Bytes `range` of the value of `key`; `None` if absent.
    fn get_range<'a>(
        &'a self,
        key: &'a str,
        range: Range<u64>,
    ) -> BoxFuture<'a, io::Result<Option<Bytes>>>;
    /// Store `value` at `key`.
    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>>;
    /// Remove `keys`; a missing key succeeds.
    fn delete<'a>(&'a self, keys: &'a [String]) -> BoxFuture<'a, io::Result<()>>;
    /// Every key under `prefix`.
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, io::Result<Vec<String>>>;
}

/// Where [`KvBlobs::new`] keeps its blobs' parts: packs kept in Restate.
pub const KV_PREFIX: &str = "v20/pack/";
/// Where checkpoints' manifests are kept, in Restate wherever the packs
/// are, in parts of 64 KiB.
pub const MANIFEST_PREFIX: &str = "v20/manifest/";
/// Bytes of a part of a blob in [`KvBlobs`]: a range read gets the parts it
/// overlaps.
pub(crate) const KV_PART: u64 = 64 << 10;

/// Blobs in a [`KvStore`] (Restate's state, beside the head and the tail),
/// each in parts of 64 KiB: `<prefix><key>/<part>`.
///
/// A put only queues its parts: they are durable with the store's next sync.
/// That is enough for a checkpoint, as a [`KvStore`] keeps its writes in
/// order: the head naming the pack, put after it, is never durable without it.
pub struct KvBlobs {
    store: Arc<dyn KvStore>,
    prefix: &'static str,
}

impl KvBlobs {
    /// Packs, under [`KV_PREFIX`].
    pub fn new(store: Arc<dyn KvStore>) -> Self {
        Self {
            store,
            prefix: KV_PREFIX,
        }
    }

    /// Manifests, under [`MANIFEST_PREFIX`].
    pub(crate) fn manifests(store: Arc<dyn KvStore>) -> Self {
        Self {
            store,
            prefix: MANIFEST_PREFIX,
        }
    }

    fn part_key(&self, key: &str, part: u64) -> String {
        format!("{}{key}/{part}", self.prefix)
    }

    /// Part `part` of the blob `key`: its bytes `part × 64 KiB` onward, up
    /// to 64 KiB; `None` if absent.
    pub(crate) async fn part(&self, key: &str, part: u64) -> io::Result<Option<Bytes>> {
        self.store.get(&self.part_key(key, part)).await
    }

    /// The blob's key, of a part's key.
    fn blob<'k>(&self, part: &'k str) -> Option<&'k str> {
        Some(part.strip_prefix(self.prefix)?.rsplit_once('/')?.0)
    }

    /// The keys of the blobs under `prefix` whose parts are among `keys`, a
    /// listing of the store.
    pub(crate) fn listed(&self, keys: &[String], prefix: &str) -> BTreeSet<String> {
        (keys.iter())
            .filter_map(|part| self.blob(part))
            .filter(|blob| blob.starts_with(prefix))
            .map(str::to_owned)
            .collect()
    }

    /// Queue the deletes of the parts among `keys`, a listing of the store,
    /// of the blobs `doomed` picks: durable with the store's next sync.
    /// Whether it queued any.
    pub(crate) fn queue_delete(
        &self,
        keys: &[String],
        doomed: impl Fn(&str) -> bool,
    ) -> io::Result<bool> {
        let mut queued = false;
        for part in keys {
            if self.blob(part).is_some_and(&doomed) {
                self.store.delete(part)?;
                queued = true;
            }
        }
        Ok(queued)
    }
}

impl BlobStore for KvBlobs {
    fn get_range<'a>(
        &'a self,
        key: &'a str,
        range: Range<u64>,
    ) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async move {
            if range.is_empty() {
                return Ok(Some(Bytes::new()));
            }
            let parts = range.start / KV_PART..range.end.div_ceil(KV_PART);
            let first = parts.start;
            let got = try_join_all(parts.map(|part| {
                let key = self.part_key(key, part);
                async move { self.store.get(&key).await }
            }))
            .await?;
            let mut out = BytesMut::new();
            for part in got {
                match part {
                    Some(part) => out.extend_from_slice(&part),
                    None => return Ok(None),
                }
            }
            let start = (range.start - first * KV_PART) as usize;
            let end = start + (range.end - range.start) as usize;
            if end > out.len() {
                return Err(io::Error::other(format!("{key} is shorter than {range:?}")));
            }
            Ok(Some(out.freeze().slice(start..end)))
        })
    }

    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            for (part, at) in (0..value.len() as u64)
                .step_by(KV_PART as usize)
                .enumerate()
            {
                let end = (at + KV_PART).min(value.len() as u64);
                let slice = value.slice(at as usize..end as usize);
                self.store.put(&self.part_key(key, part as u64), slice)?;
            }
            Ok(())
        })
    }

    /// One listing and one sync for all of `keys`.
    fn delete<'a>(&'a self, keys: &'a [String]) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            if keys.is_empty() {
                return Ok(());
            }
            let blobs: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
            self.queue_delete(&self.store.keys().await?, |blob| blobs.contains(blob))?;
            self.store.sync().await
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, io::Result<Vec<String>>> {
        Box::pin(async move {
            let keys = self.store.keys().await?;
            Ok(self.listed(&keys, prefix).into_iter().collect())
        })
    }
}

#[cfg(feature = "object-store")]
pub use object::ObjectStoreBlobs;

#[cfg(feature = "object-store")]
mod object {
    use super::BlobStore;
    use bytes::Bytes;
    use futures::{
        TryStreamExt,
        future::{BoxFuture, try_join_all},
    };
    use object_store::{Error, ObjectStore, ObjectStoreExt, path::Path};
    use std::{io, ops::Range, sync::Arc};

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

        async fn delete_one(&self, key: &str) -> io::Result<()> {
            match self.store.delete(&self.path(key)).await {
                Ok(()) | Err(Error::NotFound { .. }) => Ok(()),
                Err(error) => Err(error.into()),
            }
        }
    }

    impl BlobStore for ObjectStoreBlobs {
        fn get_range<'a>(
            &'a self,
            key: &'a str,
            range: Range<u64>,
        ) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
            Box::pin(async move {
                match self.store.get_range(&self.path(key), range).await {
                    Ok(bytes) => Ok(Some(bytes)),
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

        fn delete<'a>(&'a self, keys: &'a [String]) -> BoxFuture<'a, io::Result<()>> {
            Box::pin(async move {
                try_join_all(keys.iter().map(|key| self.delete_one(key))).await?;
                Ok(())
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
