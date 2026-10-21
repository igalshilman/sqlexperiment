use crate::cow::Golden;
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

impl GuestError {
    pub fn new(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            message: message.into(),
        }
    }
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
///
/// A call's input stays in the guest's heap until the call is answered: the
/// host reads it when it makes the call ([`crate::Run::input`]), so the
/// commit keeps the call's ID, not a copy of its input.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Operation {
    /// An extension's operation, such as `ctx.call(name, input)` ([`crate::Extensions`]).
    Call {
        id: String,
        name: String,
        /// The guest's own idempotency key, if it set one.
        #[serde(rename = "idempotencyKey", skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
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
        /// The guest's own idempotency key, if it set one.
        #[serde(rename = "idempotencyKey", skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    /// A wait for the signal `name`, `ctx.signal(name, input)`: what the
    /// object's `signal` handler gives it ([`crate::restate`]). `input` says
    /// what it waits for, as `pending` lists it.
    Signal {
        id: String,
        name: String,
        #[serde(default)]
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
            | Self::Signal { id, .. }
            | Self::Sleep { id, .. }
            | Self::Resume { id } => id,
        }
    }
    /// The key a call is made under: the guest's, if it set one, or else the
    /// operation's ID, `<run>/<n>`, the same on every attempt.
    pub fn idempotency_key(&self) -> &str {
        match self {
            Self::Call {
                idempotency_key: Some(key),
                ..
            }
            | Self::Rpc {
                idempotency_key: Some(key),
                ..
            } => key,
            _ => self.id(),
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
/// last report (the host keeps them pending; the guest keeps only a call's
/// input), and where the run stands.
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
    golden: Arc<Golden>,
    len: usize,
    changed: BTreeMap<u32, Box<[u8]>>,
}

impl Image {
    pub fn golden(meta: VmMeta, memory: Vec<u8>) -> Self {
        let len = memory.len();
        Self {
            meta,
            golden: Arc::new(Golden::new(memory)),
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
                self.golden.bytes().get(at..at + PAGE).unwrap_or(&ZERO)
            }
        }
    }
    /// Golden's page `index`, or zeros beyond it.
    pub fn original(&self, index: u32) -> &[u8] {
        static ZERO: [u8; PAGE] = [0; PAGE];
        let at = index as usize * PAGE;
        self.golden.bytes().get(at..at + PAGE).unwrap_or(&ZERO)
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
    pub fn golden_memory(&self) -> &Arc<Golden> {
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
        let golden = self.golden.bytes();
        memory[..golden.len()].copy_from_slice(golden);
        memory[golden.len()..].fill(0);
        for (index, page) in &self.changed {
            let at = *index as usize * PAGE;
            memory[at..at + PAGE].copy_from_slice(page);
        }
    }
}

/// The scope's current run, as its last seal keeps it (the part "run"):
/// what it calls, and where it stands, its pending operations the outbox the
/// host makes once the seal is written.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunRecord {
    /// The run, and the exported function it calls.
    pub run: String,
    pub handler: String,
    /// The program, and the version (of its source) this run started on.
    pub program: String,
    pub version: u64,
    /// The runtime's version: an image restores only on the runtime that made it.
    pub runtime: u64,
    /// Turns sealed, this one included.
    pub turns: u64,
    pub state: State,
}

/// The run's memory, as its last seal keeps it (the record "memory"): the
/// image is the program's golden image with the pages of `file`, the
/// scope's file over golden, that differ from it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemoryRecord {
    pub meta: VmMeta,
    pub memory_len: usize,
    /// The file the pages are in, named after its golden memory.
    pub file: String,
    /// The pages that differ from golden, as runs of `[first, count]`: the
    /// store cannot list the file's pages, so the record does. Empty until
    /// the run's first turn, and once it has settled.
    pub pages: Vec<(u32, u32)>,
}

impl MemoryRecord {
    /// The pages that differ from golden, in order.
    pub fn pages(&self) -> impl Iterator<Item = u32> + '_ {
        (self.pages.iter()).flat_map(|&(first, count)| first..first + count)
    }

    /// Record `pages` (ascending) as the ones that differ from golden.
    pub fn set_pages(&mut self, pages: impl Iterator<Item = u32>) {
        self.pages.clear();
        for page in pages {
            match self.pages.last_mut() {
                Some((first, count)) if *first + *count == page => *count += 1,
                _ => self.pages.push((page, 1)),
            }
        }
    }

    /// How many pages differ from golden.
    pub fn live_pages(&self) -> usize {
        self.pages.iter().map(|&(_, count)| count as usize).sum()
    }
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
    /// Bytes the last turn's seal logged: the pages it changed (memory and
    /// tables), compressed against what they replace.
    pub logged_bytes: usize,
    /// Bytes of the commit record it put.
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
