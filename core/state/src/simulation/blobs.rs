//! An in-memory [`BlobStore`]: S3, as tests need it.
use crate::blobs::BlobStore;
use bytes::Bytes;
use futures::future::BoxFuture;
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
};

/// Blobs in memory, each durable once its request returns; clones share
/// them.
#[derive(Clone, Default)]
pub struct MemoryBlobs {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    values: BTreeMap<String, Bytes>,
    puts: u64,
    /// Puts of a key that held a value.
    overwrites: u64,
    /// Puts left to accept before the store fails every put.
    puts_before_failing: Option<u64>,
}

impl MemoryBlobs {
    pub fn keys(&self) -> Vec<String> {
        self.inner.lock().unwrap().values.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Puts accepted.
    pub fn puts(&self) -> u64 {
        self.inner.lock().unwrap().puts
    }

    /// Puts of a key that held a value.
    pub fn overwrites(&self) -> u64 {
        self.inner.lock().unwrap().overwrites
    }

    /// Accept `puts` more puts, then fail every put; `None` accepts them all.
    pub fn fail_after(&self, puts: Option<u64>) {
        self.inner.lock().unwrap().puts_before_failing = puts;
    }
}

impl BlobStore for MemoryBlobs {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async move { Ok(self.inner.lock().unwrap().values.get(key).cloned()) })
    }

    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().unwrap();
            if let Some(left) = &mut inner.puts_before_failing {
                if *left == 0 {
                    return Err(io::Error::other("the blob store is down"));
                }
                *left -= 1;
            }
            inner.puts += 1;
            if let Some(held) = inner.values.insert(key.to_owned(), value.clone()) {
                assert_eq!(held, value, "{key} changed: a blob's key is its hash");
                inner.overwrites += 1;
            }
            Ok(())
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.inner.lock().unwrap().values.remove(key);
            Ok(())
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, io::Result<Vec<String>>> {
        Box::pin(async move {
            let inner = self.inner.lock().unwrap();
            Ok((inner.values.keys())
                .filter(|key| key.starts_with(prefix))
                .cloned()
                .collect())
        })
    }
}
