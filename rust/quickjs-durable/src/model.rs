use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

pub(crate) const PAGE: usize = 4096;
pub(crate) const MAX_MEMORY: usize = 64 * 1024 * 1024;
pub(crate) const MAX_INPUT: usize = 1024 * 1024;
// WASI clock_time_get converts milliseconds to unsigned 64-bit nanoseconds.
pub(crate) const MAX_TIME: u64 = u64::MAX / 1_000_000;

pub(crate) fn validate_time(now: u64) -> anyhow::Result<()> {
    anyhow::ensure!(now <= MAX_TIME, "Invalid event time");
    Ok(())
}

/// Host/guest results are JSON; arbitrary values remain in the QuickJS heap.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Outcome {
    Success { ok: bool, value: Value },
    Failure { ok: bool, error: GuestError },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GuestError {
    pub name: String,
    pub message: String,
}

impl Outcome {
    pub fn success(value: impl Into<Value>) -> Self {
        Self::Success {
            ok: true,
            value: value.into(),
        }
    }
    pub fn failure(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Failure {
            ok: false,
            error: GuestError {
                name: name.into(),
                message: message.into(),
            },
        }
    }
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(
                self,
                Self::Success { ok: true, .. } | Self::Failure { ok: false, .. }
            ),
            "Invalid outcome discriminator"
        );
        Ok(())
    }
}

/// Destination of a JSON RPC. `key` addresses an object/workflow instance;
/// omit it for an unkeyed service. The host owns routing and authorization.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcTarget {
    pub service: String,
    pub handler: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Guest requests and internal resume wakeups. KV requests are answered in the
/// turn; the others wait in the run's pending list, under stable IDs, for the
/// host to deliver their outcome. The KV scope is the host's, not the guest's.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Operation {
    Call {
        id: String,
        name: String,
        args: Value,
    },
    #[serde(rename = "kv.get")]
    KvGet {
        id: String,
        key: String,
    },
    #[serde(rename = "kv.set")]
    KvSet {
        id: String,
        key: String,
        value: Value,
    },
    #[serde(rename = "kv.delete")]
    KvDelete {
        id: String,
        key: String,
    },
    Rpc {
        id: String,
        target: RpcTarget,
        input: Value,
    },
    Sleep {
        id: String,
        deadline: u64,
    },
    /// Internal host-scheduled wakeup, never an application API request.
    Resume {
        id: String,
    },
}
impl Operation {
    pub fn id(&self) -> &str {
        match self {
            Self::Call { id, .. }
            | Self::KvGet { id, .. }
            | Self::KvSet { id, .. }
            | Self::KvDelete { id, .. }
            | Self::Rpc { id, .. }
            | Self::Sleep { id, .. }
            | Self::Resume { id } => id,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Waiting,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub pending: Vec<Operation>,
    pub next_operation: u64,
    pub now: u64,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
}

/// The guest's report at the end of a step: the requests it issued since its
/// last report, which it then forgets (the host keeps them pending), and
/// where the run stands.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    pub issued: Vec<Operation>,
    pub next_operation: u64,
    pub now: u64,
    pub status: Status,
    pub outcome: Option<Outcome>,
}

/// Pointers are offsets into the same pinned Wasm binary's linear memory.
/// The indirect function table is reconstructed by instantiating that binary;
/// extensions and mutable reference tables are not supported by this experiment.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct VmMeta {
    pub stack: i32,
    pub runtime: i32,
    pub context: i32,
}

/// Captured page contents. The transient tracker memory is never persisted.
pub(crate) struct Capture {
    pub meta: VmMeta,
    pub memory_len: usize,
    pub pages: Vec<(u32, Vec<u8>)>,
    pub examined_pages: usize,
}

/// A memory image: a program's golden memory, shared, and the pages that
/// differ from it. Beyond golden's length, golden reads as zeros.
#[derive(Clone)]
pub(crate) struct Image {
    pub meta: VmMeta,
    golden: Arc<Vec<u8>>,
    len: usize,
    changed: BTreeMap<u32, Box<[u8]>>,
}

impl Image {
    pub fn golden(meta: VmMeta, memory: Arc<Vec<u8>>) -> Self {
        let len = memory.len();
        Self {
            meta,
            golden: memory,
            len,
            changed: BTreeMap::new(),
        }
    }
    pub fn len(&self) -> usize {
        self.len
    }
    /// Grow to `len` bytes, of zeros.
    pub fn grow(&mut self, len: usize) {
        debug_assert!(len >= self.len);
        self.len = len;
    }
    pub fn page(&self, index: u32) -> &[u8] {
        static ZERO: [u8; PAGE] = [0; PAGE];
        match self.changed.get(&index) {
            Some(page) => page,
            None => {
                let at = index as usize * PAGE;
                self.golden.get(at..at + PAGE).unwrap_or(&ZERO)
            }
        }
    }
    /// Golden's page `index`, or zeros beyond it.
    pub fn original(&self, index: u32) -> &[u8] {
        static ZERO: [u8; PAGE] = [0; PAGE];
        let at = index as usize * PAGE;
        self.golden.get(at..at + PAGE).unwrap_or(&ZERO)
    }
    pub fn set(&mut self, index: u32, page: &[u8]) {
        if page == self.original(index) {
            self.changed.remove(&index);
        } else {
            self.changed.insert(index, page.into());
        }
    }
    /// The pages that differ from golden, in order.
    pub fn changed(&self) -> impl Iterator<Item = u32> + '_ {
        self.changed.keys().copied()
    }
    /// The golden memory this image is relative to, shared by every image of
    /// the program.
    pub fn golden_memory(&self) -> &Arc<Vec<u8>> {
        &self.golden
    }
    /// Write only the pages that differ from golden over `memory`, which
    /// holds golden already (and zeros beyond it).
    pub fn write_changed(&self, memory: &mut [u8]) {
        for (index, page) in &self.changed {
            let at = *index as usize * PAGE;
            memory[at..at + PAGE].copy_from_slice(page);
        }
    }
    /// Write the image over `memory`, of its length.
    pub fn write(&self, memory: &mut [u8]) {
        memory[..self.golden.len()].copy_from_slice(&self.golden);
        memory[self.golden.len()..].fill(0);
        for (index, page) in &self.changed {
            let at = *index as usize * PAGE;
            memory[at..at + PAGE].copy_from_slice(page);
        }
    }
}

/// A scope's current run, as of its last turn: the effects of the scope's
/// database commit record, so the turn happened exactly when its SQL (the KV
/// writes) and its pages (the chain) did.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Commit {
    /// The run, and the exported function it calls.
    pub run: String,
    pub handler: String,
    /// The program, and the version (of its source) this run started on.
    pub program: String,
    pub version: u64,
    /// The runtime's version: an image restores only on the runtime that made it.
    pub runtime: u64,
    /// Turns committed, this one included.
    pub turns: u64,
    pub meta: VmMeta,
    pub memory_len: usize,
    /// Batches in the database's chain since this run restarted it: the image
    /// is golden (zeros beyond its length) with the chain applied in order.
    /// The first holds the pages that differ from golden, each next one the
    /// pages one turn changed. Zero until the run's first turn, and once it
    /// has settled.
    pub batches: usize,
    /// Bytes of the chain's first batch, and of the others.
    pub base_bytes: usize,
    pub delta_bytes: usize,
    /// Pages that differ from golden.
    pub live_pages: usize,
    pub state: State,
}

/// A run as of its last commit, and what that turn wrote.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct View {
    pub run: String,
    pub program: String,
    pub turns: u64,
    pub state: State,
    pub memory_bytes: usize,
    /// Pages that differ from golden.
    pub live_pages: usize,
    /// Pages the last turn changed.
    pub changed_pages: usize,
    /// Pages the last turn compared: those it wrote to by default, all pages
    /// in the benchmark-only full-scan build.
    pub examined_pages: usize,
    pub batch_bytes: usize,
    pub commit_bytes: usize,
}

/// FNV-1a: a stable 64-bit fingerprint, to tell versions and runs apart. Not
/// a defense against tampering: storage is trusted.
pub(crate) fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

pub(crate) fn json_bytes<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(value)?)
}
