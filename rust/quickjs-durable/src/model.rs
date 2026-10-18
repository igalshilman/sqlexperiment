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

/// Guest requests and internal resume wakeups. KV resolves inside the transaction;
/// deferred operations retain stable IDs in both the VM and host effect state.
/// Application KV scope is supplied by the host, not selected by guest input.
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
/// Transient delivery to the VM, never an operation replay log.
pub(crate) enum Event {
    Start {
        program: String,
        input: Value,
        now: u64,
    },
    Complete {
        operation_id: String,
        outcome: Outcome,
        now: u64,
    },
}
impl Event {
    pub fn now(&self) -> u64 {
        match self {
            Self::Start { now, .. } | Self::Complete { now, .. } => *now,
        }
    }
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

/// One compressed batch holds several XOR pages: a page is in the batch the
/// run's checkpoint `batch` (its sequence) wrote, at `slot`. Each page is
/// relative to golden, never to another checkpoint's page. Short names: a
/// manifest lists every page that differs from golden.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct PageRef {
    #[serde(rename = "b")]
    pub batch: u64,
    #[serde(rename = "s")]
    pub slot: usize,
}

/// Complete immutable page index for one committed invocation. Missing entries
/// inherit golden, or zeros beyond golden length. `previous` is history only:
/// recovery does not follow it. The host transaction commits the manifest/head
/// together with application state and effects.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub format: u32,
    /// The program, and the version (of its source) this run started on.
    pub program: String,
    pub version: u64,
    /// The runtime's version: an image restores only on the runtime that made it.
    pub runtime: u64,
    pub previous: Option<String>,
    pub sequence: u64,
    pub meta: VmMeta,
    pub memory_len: usize,
    pub pages: BTreeMap<u32, PageRef>,
    pub state: State,
    pub changed_pages: usize,
    pub examined_pages: usize,
    pub new_blob_bytes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct View {
    pub checkpoint_id: String,
    pub program: String,
    pub checkpoint_sequence: u64,
    pub state: State,
    pub image_bytes: usize,
    pub changed_pages: usize,
    /// Pages whose contents were compared: dirty pages by default, all pages
    /// in the benchmark-only full-scan build. Reverted writes still count here.
    pub examined_pages: usize,
    pub override_pages: usize,
    pub new_blob_bytes: usize,
    pub manifest_bytes: usize,
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
