use super::*;

impl Snapshot {
    pub async fn open(store: Arc<dyn KvStore>) -> io::Result<Self> {
        Self::open_with_options(store, StorageOptions::default()).await
    }
    /// Exact structural reachability without reading database payloads.
    pub(crate) async fn reachable(&self) -> io::Result<BTreeSet<String>> {
        let mut live = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut pending = Vec::new();
        if self.height > 0 {
            pending.push((None, self.height));
        }
        while let Some((id, level)) = pending.pop() {
            if !visited.insert((id, level)) {
                continue;
            }
            let node = match id {
                Some(id) => {
                    live.insert(key("node", &id));
                    self.node(Some(id)).await?
                }
                None => self.root.clone(),
            };
            for child in node.into_values() {
                if level == 1 {
                    live.insert(key("block", &child));
                } else {
                    pending.push((Some(child), level - 1));
                }
            }
        }
        Ok(live)
    }
}
