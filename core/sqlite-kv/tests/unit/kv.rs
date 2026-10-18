use super::*;
use std::sync::{Arc, Mutex};

/// A queue that records its requests.
#[derive(Default)]
struct Recorder {
    log: Mutex<Vec<String>>,
}

impl Queued for Recorder {
    fn get<'a>(&'a self, _: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async { Ok(None) })
    }
    fn queue(&self, key: &str, value: Option<Bytes>) -> io::Result<()> {
        let kind = if value.is_some() { "put" } else { "delete" };
        self.log.lock().unwrap().push(format!("{kind} {key}"));
        Ok(())
    }
    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async {
            self.log.lock().unwrap().push("sync".into());
            Ok(())
        })
    }
}

/// A write waits for a sync after it; writes issued together share one.
#[test]
fn writes_issued_together_share_a_sync() -> io::Result<()> {
    futures::executor::block_on(async {
        let store = Arc::new(Synced::new(Recorder::default()));
        store.put("a", Bytes::new()).await?;
        let (b, c) = futures::join!(store.put("b", Bytes::new()), store.delete("c"));
        b?;
        c?;
        let log = store.queue().log.lock().unwrap().clone();
        assert_eq!(log, ["put a", "sync", "put b", "delete c", "sync"]);
        Ok(())
    })
}
