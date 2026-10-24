//! Shared by the simulation tests: a seeded random source, a file base, a
//! session holding a state's files, and a reference model of its content.
#![allow(dead_code)]
use bytes::Bytes;
use state::{
    Base, File, FlushReport, Mutation, PAGE_SIZE, Page, Result, State, kv::KvStore,
    simulation::Setup,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

/// A deterministic random source (splitmix64).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `n` (0 when `n` is 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    /// True `percent` times in a hundred.
    pub fn percent(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

/// A page numbered `n` in its first eight bytes, zeros after: compressible.
pub fn page(n: u64) -> Page {
    let mut page = vec![0; PAGE_SIZE];
    page[..8].copy_from_slice(&n.to_le_bytes());
    page.into()
}

/// A page of mixed entropy: random bytes for a random part of it.
pub fn varied(rng: &mut Rng) -> Page {
    let mut page = page(rng.next()).to_vec();
    let noisy = rng.below(PAGE_SIZE as u64) as usize;
    let at = rng.below((PAGE_SIZE - noisy) as u64 + 1) as usize;
    page[at..at + noisy].copy_from_slice(&rng.bytes(noisy));
    page.into()
}

/// The base of the file `vm`: a golden image of a few pages.
pub struct Golden {
    pages: Vec<Vec<u8>>,
}

impl Base for Golden {
    fn id(&self) -> u64 {
        7
    }
    fn page(&self, index: u64) -> Option<&[u8]> {
        self.pages.get(index as usize).map(Vec::as_slice)
    }
}

/// The one base the tests use.
pub fn golden() -> Arc<Golden> {
    static GOLDEN: OnceLock<Arc<Golden>> = OnceLock::new();
    GOLDEN
        .get_or_init(|| {
            let pages = (0..6u8)
                .map(|n| (0..PAGE_SIZE).map(|i| (i as u8 / 3) ^ n).collect())
                .collect();
            Arc::new(Golden { pages })
        })
        .clone()
}

/// The paged files: `vm` has the base [`golden`].
pub const FILES: [&str; 3] = ["db", "aux", "vm"];
/// The records.
pub const RECORDS: [&str; 3] = ["progress", "outbox", "state"];
/// The logs.
pub const LOGS: [&str; 2] = ["turns", "events"];

/// A change to a state (in a mutation) and to its model alike.
#[derive(Clone, Debug)]
pub enum Op {
    Set(&'static str, Bytes),
    Write(&'static str, u64, Option<Page>),
    SetLen(&'static str, u64),
    Append(&'static str, Bytes),
    /// Drop the log's entries before an index, at most its end as flushed
    /// (see [`Model::truncation`]).
    Truncate(&'static str, u64),
}

impl Op {
    /// A random change: records and log entries small and over an inline
    /// budget of a few hundred bytes, pages written, holes, and lengths
    /// shrunk and grown.
    pub fn random(rng: &mut Rng) -> Self {
        let file = rng.pick(&FILES);
        let len = |rng: &mut Rng| match rng.below(4) {
            0 => 0,
            1 => rng.below(40) as usize,
            2 => 100 + rng.below(400) as usize,
            _ => 1000 + rng.below(3000) as usize,
        };
        match rng.below(12) {
            0..=1 => {
                let len = len(rng);
                Self::Set(rng.pick(&RECORDS), rng.bytes(len).into())
            }
            10..=11 => {
                let len = len(rng);
                Self::Append(rng.pick(&LOGS), rng.bytes(len).into())
            }
            2 => Self::SetLen(file, 512 * rng.below(8 * 20)),
            3 => Self::Write(file, rng.below(20), None),
            4 if file == "vm" => {
                // A page as its base has it: stored as a hole.
                let index = rng.below(6);
                let page = golden().page(index).map(Page::copy_from_slice);
                Self::Write(file, index, page)
            }
            _ => Self::Write(file, rng.below(20), Some(varied(rng))),
        }
    }
}

/// A state and handles on its files.
pub struct Session {
    pub state: State,
    files: BTreeMap<&'static str, File>,
}

impl Session {
    /// Open the state, and its files (`vm` with its base).
    pub async fn open(store: Arc<dyn KvStore>, setup: impl Into<Setup>) -> Result<Self> {
        Self::of(setup.into().open(store).await?)
    }

    /// Open a reader beside the owner (see [`state::Open::reader`]).
    pub async fn reader(store: Arc<dyn KvStore>, setup: impl Into<Setup>) -> Result<Self> {
        Self::of(setup.into().open(store).reader().await?)
    }

    fn of(state: State) -> Result<Self> {
        let mut files = BTreeMap::new();
        for name in FILES {
            let file = match name {
                "vm" => state.file_with_base(name, golden())?,
                _ => state.file(name)?,
            };
            files.insert(name, file);
        }
        Ok(Self { state, files })
    }

    pub fn file(&self, name: &str) -> &File {
        &self.files[name]
    }

    /// Make the change `op` in `mutation`, a mutation of this state.
    pub fn apply(&self, mutation: &mut Mutation, op: &Op) -> Result<()> {
        match op {
            Op::Set(name, value) => mutation.set(&self.state.record(name), value.clone()),
            Op::Write(name, index, image) => mutation.write(self.file(name), *index, image.clone()),
            Op::SetLen(name, len) => mutation.set_len(self.file(name), *len),
            Op::Append(name, entry) => {
                mutation.append(&self.state.log(name), entry.clone())?;
                Ok(())
            }
            Op::Truncate(name, before) => mutation.truncate(&self.state.log(name), *before),
        }
    }

    /// Flush `ops`, in a mutation of their own.
    pub async fn flush(&self, ops: &[Op]) -> Result<FlushReport> {
        let mut mutation = self.state.mutation();
        for op in ops {
            self.apply(&mut mutation, op)?;
        }
        mutation.flush().await
    }
}

/// What a state should hold, as last flushed: its records, its files'
/// pages, and its logs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Model {
    records: BTreeMap<String, Bytes>,
    files: BTreeMap<String, FileModel>,
    logs: BTreeMap<String, LogModel>,
}

/// A log's first index and its entries from there.
#[derive(Clone, Debug, Default, PartialEq)]
struct LogModel {
    first: u64,
    entries: Vec<Bytes>,
}

/// A file's length and pages; an absent page is a hole.
#[derive(Clone, Debug, Default, PartialEq)]
struct FileModel {
    len: u64,
    pages: BTreeMap<u64, Page>,
}

const PAGE: u64 = PAGE_SIZE as u64;

impl Model {
    pub fn apply(&mut self, op: &Op) {
        match op {
            Op::Set(name, value) => {
                self.records.insert(name.to_string(), value.clone());
            }
            Op::Write(name, index, image) => {
                let file = self.files.entry(name.to_string()).or_default();
                file.len = file.len.max((index + 1) * PAGE);
                match image {
                    Some(image) => file.pages.insert(*index, image.clone()),
                    None => file.pages.remove(index),
                };
            }
            Op::SetLen(name, len) => {
                let file = self.files.entry(name.to_string()).or_default();
                let kept = len.div_ceil(PAGE);
                file.pages.retain(|&index, _| index < kept);
                file.len = *len;
            }
            Op::Append(name, entry) => {
                let log = self.logs.entry(name.to_string()).or_default();
                log.entries.push(entry.clone());
            }
            Op::Truncate(name, before) => {
                let log = self.logs.entry(name.to_string()).or_default();
                let dropped = before.saturating_sub(log.first) as usize;
                log.entries.drain(..dropped.min(log.entries.len()));
                log.first = log.first.max(*before);
            }
        }
    }

    /// A truncation of a random log valid in any mutation of the state
    /// this model holds: of the log as flushed, up to its end.
    pub fn truncation(&self, rng: &mut Rng) -> Op {
        let name = rng.pick(&LOGS);
        let (first, next) = self.log(name);
        Op::Truncate(name, first + rng.below(next - first + 1))
    }

    /// The first index and the next of the log `name`.
    pub fn log(&self, name: &str) -> (u64, u64) {
        self.logs.get(name).map_or((0, 0), |log| {
            (log.first, log.first + log.entries.len() as u64)
        })
    }

    pub fn applied(&self, ops: &[Op]) -> Self {
        let mut model = self.clone();
        for op in ops {
            model.apply(op);
        }
        model
    }

    /// What reading page `index` of the file `name` returns.
    pub fn page(&self, name: &str, index: u64) -> Option<Page> {
        let file = self.files.get(name)?;
        if index * PAGE >= file.len {
            return None;
        }
        let hole = || match name {
            "vm" => golden().page(index).map(Page::copy_from_slice),
            _ => None,
        };
        file.pages.get(&index).cloned().or_else(hole)
    }

    pub fn len(&self, name: &str) -> u64 {
        self.files.get(name).map_or(0, |file| file.len)
    }

    pub fn record(&self, name: &str) -> Bytes {
        self.records.get(name).cloned().unwrap_or_default()
    }

    /// How `session` differs from the model; `None` when it reads the same.
    /// A session reads its state as last flushed, so compare after a flush.
    pub async fn differs(&self, session: &Session) -> Result<Option<String>> {
        for name in RECORDS {
            let held = session.state.record(name).get();
            if held != self.record(name) {
                return Ok(Some(format!("record {name}: {} bytes", held.len())));
            }
        }
        for name in LOGS {
            let log = session.state.log(name);
            let model = self.logs.get(name).cloned().unwrap_or_default();
            if (log.first(), log.entries()) != (model.first, model.entries) {
                let (first, next) = (log.first(), log.next());
                return Ok(Some(format!(
                    "log {name}: {first}..{next}, not {:?}",
                    self.log(name)
                )));
            }
        }
        for name in FILES {
            let file = session.file(name);
            if file.len() != self.len(name) {
                let (held, len) = (file.len(), self.len(name));
                return Ok(Some(format!("{name}: {held} bytes, not {len}")));
            }
            for index in 0..=self.len(name).div_ceil(PAGE) {
                let (held, expected) = (file.read(index).await?, self.page(name, index));
                if held != expected {
                    let start = |page: Option<Page>| page.map(|page| page[..4].to_vec());
                    let (held, expected) = (start(held), start(expected));
                    return Ok(Some(format!(
                        "{name}: page {index} starts {held:02x?}, not {expected:02x?}"
                    )));
                }
            }
        }
        Ok(None)
    }

    /// Whether `session` reads as the model.
    pub async fn matches(&self, session: &Session) -> Result<bool> {
        Ok(self.differs(session).await?.is_none())
    }
}
