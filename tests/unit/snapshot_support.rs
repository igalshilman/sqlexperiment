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
        if self.head.height > 0 {
            pending.push((None, self.head.height));
        }
        while let Some((id, level)) = pending.pop() {
            if !visited.insert((id, level)) {
                continue;
            }
            let node = match id {
                Some(id) => {
                    live.insert(Kind::Node.key(id));
                    self.node(Some(id)).await?
                }
                None => self.head.root.clone(),
            };
            for child in node.into_values() {
                if level == 1 {
                    live.insert(Kind::Block.key(child));
                } else {
                    pending.push((Some(child), level - 1));
                }
            }
        }
        if self.head.log_start != 0 {
            let segments = self.head.log_start..self.head.log_start + self.log.count;
            live.extend(segments.map(|seq| Kind::Log.key(seq)));
        }
        Ok(live)
    }
}
