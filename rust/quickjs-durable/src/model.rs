use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

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

pub(crate) struct Image {
    pub meta: VmMeta,
    pub memory: Vec<u8>,
}

/// A scope's commit: its current run, as of its last turn. It is the one record
/// a turn writes last, after the batch of pages it references: the turn
/// happened once it is stored. What follows it (the turn's KV writes, and the
/// batches no longer referenced) is applied after it, and again by the next
/// attempt that reads it, so a crash in between loses nothing.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Commit {
    pub run: String,
    /// The program, and the version (of its source) this run started on.
    pub program: String,
    pub version: u64,
    /// The runtime's version: an image restores only on the runtime that made it.
    pub runtime: u64,
    /// Turns committed, this one included.
    pub turns: u64,
    pub meta: VmMeta,
    pub memory_len: usize,
    /// The image, as batches to apply in order over golden (zeros beyond its
    /// length): the first XORed against golden, each next one against the
    /// image before it. Empty once the run has settled.
    pub chain: Vec<u64>,
    /// Bytes of the chain's first batch, and of the others.
    pub base_bytes: usize,
    pub delta_bytes: usize,
    /// Pages that differ from golden.
    pub live_pages: usize,
    /// The number of the next batch a turn writes.
    pub next_batch: u64,
    pub state: State,
    /// The turn's KV writes and deletes, applied after the commit.
    pub writes: BTreeMap<String, Value>,
    pub deletes: Vec<String>,
    /// Batches no longer referenced, deleted after the commit.
    pub garbage: Vec<u64>,
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
