//! The runtime's data: what crosses between host and guest ([`Operation`],
//! [`Outcome`], the guest's [`Report`]), what a seal keeps of a run
//! ([`RunRecord`], [`MemoryRecord`]), what the host learns of it ([`View`]),
//! memory images ([`Image`]), and the limits every part checks.
use crate::cow::Golden;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{cell::RefCell, collections::BTreeMap, sync::Arc};
use zstd::zstd_safe;

pub(crate) const PAGE: usize = 4096;
/// A page of zeros: what memory past golden's end reads as.
pub(crate) static ZERO_PAGE: [u8; PAGE] = [0; PAGE];
pub(crate) const MAX_MEMORY: usize = 64 * 1024 * 1024;
pub(crate) const MAX_INPUT: usize = 1024 * 1024;
// WASI clock_time_get converts milliseconds to unsigned 64-bit nanoseconds.
pub(crate) const MAX_TIME: u64 = u64::MAX / 1_000_000;

pub(crate) fn validate_time(now: u64) -> anyhow::Result<()> {
    anyhow::ensure!(now <= MAX_TIME, "Invalid event time");
    Ok(())
}

/// How a request settled, as the guest sees it: JSON either way. Other
/// values stay in the guest's heap.
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

    /// A `TypeError`: the program called something the wrong way.
    pub fn type_error(message: impl Into<String>) -> Self {
        Self::new("TypeError", message)
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
            error: GuestError::new(name, message),
        }
    }
    /// The outcome of what answered with `result`: its value, or its error.
    pub fn from_result(result: Result<Value, GuestError>) -> Self {
        match result {
            Ok(value) => Self::success(value),
            Err(error) => Self::Failure { ok: false, error },
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

/// The run's progress between turns, as the seal keeps it: its pending
/// operations (the outbox the host makes once the seal is flushed), the
/// number of the next, its clock, and where it stands.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
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

/// What an image keeps outside memory: the stack pointer, and QuickJS's
/// runtime and context (0 for Python). Pointers are offsets into the same
/// pinned Wasm binary's linear memory; instantiating that binary rebuilds the
/// indirect function table, which the snapshot ABI forbids changing.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct VmMeta {
    pub stack: i32,
    pub runtime: i32,
    pub context: i32,
}

/// What a turn changed against its run's image ([`crate::vm::Vm::capture`]):
/// the metadata, the memory's length, and each page that differs, by index.
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
    /// Page `index` of the image.
    pub fn page(&self, index: u32) -> &[u8] {
        match self.changed.get(&index) {
            Some(page) => page,
            None => self.original(index),
        }
    }
    /// Golden's page `index`, or zeros beyond it.
    pub fn original(&self, index: u32) -> &[u8] {
        let at = index as usize * PAGE;
        self.golden.bytes().get(at..at + PAGE).unwrap_or(&ZERO_PAGE)
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
        self.write_changed(memory);
    }
}

/// The scope's current run, as its last seal keeps it (the record "run"):
/// what it calls, and its progress, its pending operations the outbox the
/// host makes once the seal is flushed.
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
    /// Its progress (the JSON field `state`).
    pub state: Progress,
}

/// The run's memory, as its last seal keeps it (the record "memory"): the
/// image is the program's golden image with the pages of `file`, the
/// scope's file over golden, as the last fold left them, and the turns since
/// replayed over them, each an entry of the log "memory/turns" (see
/// [`TurnEntry`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemoryRecord {
    pub meta: VmMeta,
    pub memory_len: usize,
    /// The file the folded pages are in, named after its golden memory.
    pub file: String,
    /// The pages that differ from golden, as runs of `[first, count]`.
    /// Empty until the run's first turn, and once it has settled.
    pub pages: Vec<(u32, u32)>,
    /// The pages of `file` that differ from golden, as runs: the store
    /// cannot list a file's pages, so the record does.
    pub folded: Vec<(u32, u32)>,
}

impl MemoryRecord {
    /// Record `pages` (ascending) as the ones that differ from golden.
    pub fn set_pages(&mut self, pages: impl Iterator<Item = u32>) {
        self.pages = runs(pages);
    }

    /// The pages of the file that differ from golden, in order.
    pub fn folded(&self) -> impl Iterator<Item = u32> + '_ {
        unrun(&self.folded)
    }

    /// Record `pages` (ascending) as the file's that differ from golden.
    pub fn set_folded(&mut self, pages: impl Iterator<Item = u32>) {
        self.folded = runs(pages);
    }

    /// How many pages differ from golden.
    pub fn live_pages(&self) -> usize {
        self.pages.iter().map(|&(_, count)| count as usize).sum()
    }
}

/// Ascending `pages` as runs of `[first, count]`.
fn runs(pages: impl Iterator<Item = u32>) -> Vec<(u32, u32)> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for page in pages {
        match runs.last_mut() {
            Some((first, count)) if *first + *count == page => *count += 1,
            _ => runs.push((page, 1)),
        }
    }
    runs
}

fn unrun(runs: &[(u32, u32)]) -> impl Iterator<Item = u32> + '_ {
    runs.iter().flat_map(|&(first, count)| first..first + count)
}

/// What a turn changed in the memory, as an entry of the log
/// "memory/turns": the pages it changed, compressed as one Zstd frame with
/// their images before the turn as its prefix, so a page costs about what
/// changed in it, wherever it moved. Replayed in order over the image the
/// last fold left, the log's entries make the sealed image.
///
/// Encoded as the count of pages, their indexes (each a little-endian
/// `u32`), then the frame.
#[derive(Default)]
pub(crate) struct TurnEntry {
    pub indexes: Vec<u32>,
    before: Vec<u8>,
    after: Vec<u8>,
}

/// A decoded [`TurnEntry`]: its pages, and the frame of their new images.
pub(crate) struct LoggedTurn {
    pub indexes: Vec<u32>,
    frame: Vec<u8>,
}

impl TurnEntry {
    /// Page `index` changed from `before` to `after`.
    pub fn push(&mut self, index: u32, before: &[u8], after: &[u8]) {
        self.indexes.push(index);
        self.before.extend_from_slice(before);
        self.after.extend_from_slice(after);
    }

    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        let mut bytes = header(&self.indexes);
        let mut frame = Vec::with_capacity(zstd_safe::compress_bound(self.after.len()));
        COMPRESS.with_borrow_mut(|cctx| {
            let compressed = (cctx.set_parameter(zstd_safe::CParameter::CompressionLevel(3)))
                .and_then(|_| cctx.ref_prefix(referenced(&self.before)))
                .and_then(|_| cctx.compress2(&mut frame, &self.after));
            // The prefix goes with the session: the next frame has its own.
            if cctx
                .reset(zstd_safe::ResetDirective::SessionAndParameters)
                .is_err()
            {
                *cctx = zstd_safe::CCtx::create();
            }
            compressed.map_err(zstd_error)
        })?;
        bytes.extend(frame);
        Ok(bytes)
    }
}

/// The count of `indexes`, then each, as little-endian `u32`s.
fn header(indexes: &[u32]) -> Vec<u8> {
    let count = indexes.len() as u32;
    let mut bytes = count.to_le_bytes().to_vec();
    bytes.extend(indexes.iter().flat_map(|index| index.to_le_bytes()));
    bytes
}

impl LoggedTurn {
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        let u32_at = |at: usize| {
            let word = bytes.get(at..at + 4).context("A truncated turn entry")?;
            anyhow::Ok(u32::from_le_bytes(word.try_into().unwrap()))
        };
        let count = u32_at(0)? as usize;
        let header_len = 4 + count.checked_mul(4).context("A turn entry too large")?;
        let indexes = (0..count)
            .map(|i| u32_at(4 + i * 4))
            .collect::<anyhow::Result<_>>()?;
        let frame = bytes.get(header_len..).context("A truncated turn entry")?;
        Ok(Self {
            indexes,
            frame: frame.to_vec(),
        })
    }

    /// Apply the turn to `image`, as it was before the turn.
    pub fn replay(&self, image: &mut Image) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.indexes.len() * PAGE <= image.len(),
            "A turn entry of more pages than memory"
        );
        let mut before = Vec::with_capacity(self.indexes.len() * PAGE);
        for &index in &self.indexes {
            anyhow::ensure!(
                (index as usize + 1) * PAGE <= image.len(),
                "Page beyond memory"
            );
            before.extend_from_slice(image.page(index));
        }
        let mut after = Vec::with_capacity(before.len());
        DECOMPRESS.with_borrow_mut(|dctx| {
            let decompressed = (dctx
                .set_parameter(zstd_safe::DParameter::WindowLogMax(MAX_WINDOW_LOG)))
            .and_then(|_| dctx.ref_prefix(referenced(&before)))
            .and_then(|_| dctx.decompress(&mut after, &self.frame));
            if dctx
                .reset(zstd_safe::ResetDirective::SessionAndParameters)
                .is_err()
            {
                *dctx = zstd_safe::DCtx::create();
            }
            decompressed.map_err(zstd_error)
        })?;
        anyhow::ensure!(
            after.len() == before.len(),
            "A turn entry of the wrong length"
        );
        for (&index, page) in self.indexes.iter().zip(after.chunks(PAGE)) {
            image.set(index, page);
        }
        Ok(())
    }
}

thread_local! {
    /// Making a context costs several times what coding a page with it does.
    static COMPRESS: RefCell<zstd_safe::CCtx<'static>> = RefCell::new(zstd_safe::CCtx::create());
    static DECOMPRESS: RefCell<zstd_safe::DCtx<'static>> = RefCell::new(zstd_safe::DCtx::create());
}

/// Extend `prefix`'s lifetime to a context's: the callers above reset the
/// context (or replace it, if that fails) before they return, which drops
/// the prefix.
fn referenced(prefix: &[u8]) -> &'static [u8] {
    // SAFETY: the context references the prefix only until its reset, which
    // every path through `encode` and `replay` reaches, and nothing reads the
    // prefix after it.
    unsafe { std::mem::transmute::<&[u8], &'static [u8]>(prefix) }
}

fn zstd_error(code: zstd_safe::ErrorCode) -> anyhow::Error {
    anyhow::anyhow!("Zstd: {}", zstd_safe::get_error_name(code))
}

/// The largest window a turn entry's frame needs: its pages before and
/// after, up to the log's bytes (see `FOLD_BYTES`) each.
const MAX_WINDOW_LOG: u32 = 24;

/// A run as of its last seal, and what that turn wrote.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct View {
    pub run: String,
    pub program: String,
    pub turns: u64,
    /// Where the run stands.
    pub state: Progress,
    pub memory_bytes: usize,
    /// Pages that differ from golden.
    pub live_pages: usize,
    /// Pages the last turn changed.
    pub changed_pages: usize,
    /// Pages the last turn compared: those it wrote to by default, all pages
    /// in the benchmark-only full-scan build.
    pub examined_pages: usize,
    /// Bytes of the blobs the last turn's flush put: the chunks of pages it
    /// changed (memory and tables), compressed against what they replace.
    pub blob_bytes: usize,
    /// Bytes of the root it put.
    pub root_bytes: usize,
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
