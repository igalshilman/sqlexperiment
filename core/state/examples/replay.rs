//! Experiment: replay the flushes a workload made (`STATE_TRACE`, written by
//! `core/sql/benches/cow_compare.rs` with `TRACE_DIR`) through models of
//! copy-on-write layouts, and print the bytes each would write per flush.
//!
//! ```sh
//! cargo run --release --example replay -- <trace>...
//! ```
use std::collections::{BTreeMap, BTreeSet};
use zstd_safe::{CCtx, CParameter};

const PAGE: usize = 4096;

#[derive(Default)]
struct Flush {
    sizes: BTreeMap<String, u64>,
    pages: BTreeMap<(String, u64), Vec<u8>>,
    records: BTreeMap<String, Vec<u8>>,
    /// The measured flushes start here.
    mark: bool,
}

fn parse(bytes: &[u8]) -> Vec<Flush> {
    let mut flushes: Vec<Flush> = Vec::new();
    let mut at = 0;
    let u32_ = |at: &mut usize| {
        let v = u32::from_le_bytes(bytes[*at..*at + 4].try_into().unwrap());
        *at += 4;
        v as usize
    };
    let u64_ = |at: &mut usize| {
        let v = u64::from_le_bytes(bytes[*at..*at + 8].try_into().unwrap());
        *at += 8;
        v
    };
    let mut mark = false;
    while at < bytes.len() {
        let tag = bytes[at];
        at += 1;
        let name = |at: &mut usize| {
            let n = u32_(at);
            *at += n;
            String::from_utf8(bytes[*at - n..*at].to_vec()).unwrap()
        };
        match tag {
            b'M' => mark = true,
            b'F' => flushes.push(Flush {
                mark: std::mem::take(&mut mark),
                ..Default::default()
            }),
            b'S' => {
                let file = name(&mut at);
                let size = u64_(&mut at);
                flushes.last_mut().unwrap().sizes.insert(file, size);
            }
            b'P' => {
                let file = name(&mut at);
                let page = u64_(&mut at);
                let n = u32_(&mut at);
                let image = bytes[at..at + n].to_vec();
                at += n;
                flushes.last_mut().unwrap().pages.insert((file, page), image);
            }
            b'R' => {
                let record = name(&mut at);
                let n = u32_(&mut at);
                let value = bytes[at..at + n].to_vec();
                at += n;
                flushes.last_mut().unwrap().records.insert(record, value);
            }
            _ => panic!("bad tag {tag}"),
        }
    }
    flushes
}

/// The size of `input` as one zstd frame (level 3) against the raw prefix `dict`.
fn zstd(input: &[u8], dict: &[u8]) -> usize {
    if input.is_empty() {
        return 0;
    }
    let mut cctx = CCtx::create();
    cctx.set_parameter(CParameter::CompressionLevel(3)).unwrap();
    cctx.set_parameter(CParameter::ChecksumFlag(true)).unwrap();
    // Reach back across a whole-database prefix.
    cctx.set_parameter(CParameter::WindowLog(26)).unwrap();
    cctx.ref_prefix(dict).unwrap();
    let mut out = Vec::with_capacity(zstd_safe::compress_bound(input.len()));
    cctx.compress2(&mut out, input).unwrap()
}

/// The current images: file → page → image (absent: zeros).
#[derive(Default, Clone)]
struct Image {
    pages: BTreeMap<(String, u64), Vec<u8>>,
    sizes: BTreeMap<String, u64>,
}

impl Image {
    fn apply(&mut self, flush: &Flush) {
        for (file, size) in &flush.sizes {
            self.sizes.insert(file.clone(), *size);
            let pages = size.div_ceil(PAGE as u64);
            self.pages.retain(|(f, p), _| f != file || *p < pages);
        }
        for (key, image) in &flush.pages {
            match image.is_empty() {
                true => self.pages.remove(key),
                false => self.pages.insert(key.clone(), image.clone()),
            };
        }
    }

    fn page(&self, file: &str, page: u64) -> &[u8] {
        static ZEROS: [u8; PAGE] = [0; PAGE];
        (self.pages.get(&(file.to_string(), page))).map_or(&ZEROS, |v| v)
    }

    /// Chunk `chunk` of `file`: `c` pages, cut at the file's end.
    fn chunk(&self, file: &str, chunk: u64, c: u64) -> Vec<u8> {
        let pages = self.sizes.get(file).copied().unwrap_or(0).div_ceil(PAGE as u64);
        let mut out = Vec::new();
        for page in chunk * c..((chunk + 1) * c).min(pages) {
            out.extend_from_slice(self.page(file, page));
        }
        out
    }

    fn chunks(&self, c: u64) -> BTreeSet<(String, u64)> {
        let mut out = BTreeSet::new();
        for (file, size) in &self.sizes {
            for chunk in 0..size.div_ceil(PAGE as u64).div_ceil(c) {
                out.insert((file.clone(), chunk));
            }
        }
        out
    }
}

fn records(flush: &Flush) -> Vec<u8> {
    flush.records.values().flatten().copied().collect()
}

/// What a model wrote over the measured flushes.
#[derive(Default)]
struct Cost {
    bytes: usize,
    /// Bytes written by rebases (full chunks).
    rebase_bytes: usize,
    rebases: usize,
    puts: usize,
    /// Root bytes for the page/chunk map, estimated (3 B per entry).
    map: usize,
    /// Stored bytes at the end.
    stored: usize,
    /// Bytes a cold read of one page fetches, on average over the chunks.
    cold: usize,
}

/// Each flush one frame: its changed pages, then its records; no dictionary.
fn page_cow(base: &Image, flushes: &[&Flush]) -> Cost {
    let mut cost = Cost::default();
    let mut image = base.clone();
    for flush in flushes {
        image.apply(flush);
        let mut input: Vec<u8> = flush.pages.values().flatten().copied().collect();
        input.extend(records(flush));
        cost.bytes += zstd(&input, &[]);
        cost.puts += 2;
    }
    cost.map = image.pages.len() * 3;
    cost
}

/// The log's compression, as a bound: each flush's pages against their
/// previous images, its records in the same frame; no checkpoints.
fn delta_chain(base: &Image, flushes: &[&Flush]) -> Cost {
    let mut cost = Cost::default();
    let mut image = base.clone();
    for flush in flushes {
        let mut dict = Vec::new();
        for (file, page) in flush.pages.keys() {
            dict.extend_from_slice(image.page(file, *page));
        }
        image.apply(flush);
        let mut input: Vec<u8> = flush.pages.values().flatten().copied().collect();
        input.extend(records(flush));
        cost.bytes += zstd(&input, &dict);
    }
    cost
}

struct ChunkState {
    base: Vec<u8>,
    base_len: usize,
    delta_len: usize,
}

/// Chunks of `c` pages, each a base (a frame of its own) and a delta (the
/// chunk now, against its base: cumulative). A flush writes each touched
/// chunk's delta, or a new base once the delta passes `ratio` of the base's
/// size. `joint`: one frame per flush of every delta and the records,
/// against the touched chunks' bases (else one frame per delta, and one of
/// the records).
fn chunk_delta(base: &Image, flushes: &[&Flush], c: u64, ratio: f64, joint: bool) -> Cost {
    let mut cost = Cost::default();
    let mut image = base.clone();
    let mut chunks: BTreeMap<(String, u64), ChunkState> = BTreeMap::new();
    for (file, chunk) in image.chunks(c) {
        let bytes = image.chunk(&file, chunk, c);
        let base_len = zstd(&bytes, &[]);
        chunks.insert((file, chunk), ChunkState { base: bytes, base_len, delta_len: 0 });
    }
    for flush in flushes {
        image.apply(flush);
        let touched: BTreeSet<(String, u64)> =
            flush.pages.keys().map(|(f, p)| (f.clone(), p / c)).collect();
        let (mut dict, mut input) = (Vec::new(), Vec::new());
        for (file, chunk) in touched {
            let now = image.chunk(&file, chunk, c);
            let state = chunks.entry((file, chunk)).or_insert(ChunkState {
                base: Vec::new(),
                base_len: 0,
                delta_len: 0,
            });
            let delta = zstd(&now, &state.base);
            if state.base_len == 0 || delta as f64 > ratio * state.base_len as f64 {
                state.base_len = zstd(&now, &[]);
                state.base = now;
                state.delta_len = 0;
                cost.bytes += state.base_len;
                cost.rebase_bytes += state.base_len;
                cost.rebases += 1;
                cost.puts += 1;
                continue;
            }
            state.delta_len = delta;
            if joint {
                dict.extend_from_slice(&state.base);
                input.extend_from_slice(&now);
            } else {
                cost.bytes += delta;
                cost.puts += 1;
            }
        }
        input.extend(records(flush));
        match joint {
            true => cost.bytes += zstd(&input, &dict),
            false => cost.bytes += zstd(&records(flush), &[]),
        }
        cost.puts += 2;
    }
    cost.map = chunks.len() * 2 * 3;
    cost.stored = chunks.values().map(|s| s.base_len + s.delta_len).sum();
    cost.cold = cost.stored / chunks.len().max(1);
    cost
}

/// Pages copied on write against their chunk's base: a flush writes one
/// frame of its changed pages and records, against the bases of their
/// chunks, and the root maps each page changed since its chunk's base. A
/// chunk rebases (a frame of its own) once its pages' frames, each priced
/// alone against its base, pass `ratio` of the base's size.
fn page_over_base(base: &Image, flushes: &[&Flush], c: u64, ratio: f64, budget: usize) -> Cost {
    let mut cost = Cost::default();
    let mut image = base.clone();
    let mut bases: BTreeMap<(String, u64), (Vec<u8>, usize)> = BTreeMap::new();
    // Each chunk's overridden pages and what each costs alone.
    let mut overrides: BTreeMap<(String, u64), BTreeMap<u64, usize>> = BTreeMap::new();
    for (file, chunk) in image.chunks(c) {
        let bytes = image.chunk(&file, chunk, c);
        let len = zstd(&bytes, &[]);
        bases.insert((file, chunk), (bytes, len));
    }
    for flush in flushes {
        let mut dict = Vec::new();
        let mut input = Vec::new();
        image.apply(flush);
        let mut touched = BTreeSet::new();
        for (file, page) in flush.pages.keys() {
            let key = (file.clone(), page / c);
            touched.insert(key.clone());
            let page_now = image.page(file, *page).to_vec();
            let Some((base, _)) = bases.get(&key) else {
                input.extend_from_slice(&page_now);
                continue;
            };
            static ZEROS: [u8; PAGE] = [0; PAGE];
            let at = (page - key.1 * c) as usize * PAGE;
            let page_base = base.get(at..at + PAGE).unwrap_or(&ZEROS);
            let alone = zstd(&page_now, page_base);
            dict.extend_from_slice(page_base);
            input.extend_from_slice(&page_now);
            overrides.entry(key).or_default().insert(*page, alone);
        }
        input.extend(records(flush));
        cost.bytes += zstd(&input, &dict);
        cost.puts += 2;
        for key in touched {
            let over: usize = overrides.get(&key).map_or(0, |o| o.values().sum());
            let base_len = bases.get(&key).map_or(0, |b| b.1);
            if base_len == 0 || over as f64 > ratio * base_len as f64 {
                let bytes = image.chunk(&key.0, key.1, c);
                let len = zstd(&bytes, &[]);
                bases.insert(key.clone(), (bytes, len));
                overrides.remove(&key);
                cost.bytes += len;
                cost.rebase_bytes += len;
                cost.rebases += 1;
                cost.puts += 1;
            }
        }
        // The root's map: past `budget` entries, rebase the chunks with the
        // most until half of it is left.
        let mut entries: usize = overrides.values().map(|o| o.len()).sum();
        if entries > budget {
            let mut by_count: Vec<_> = overrides.iter().map(|(k, o)| (o.len(), k.clone())).collect();
            by_count.sort();
            while entries > budget / 2 {
                let (count, key) = by_count.pop().unwrap();
                let bytes = image.chunk(&key.0, key.1, c);
                let len = zstd(&bytes, &[]);
                bases.insert(key.clone(), (bytes, len));
                overrides.remove(&key);
                entries -= count;
                cost.bytes += len;
                cost.rebase_bytes += len;
                cost.rebases += 1;
            }
        }
        // The root, rewritten: about 2.5 B a map entry, 1 B a chunk.
        cost.bytes += entries * 5 / 2 + bases.len();
    }
    cost.map = overrides.values().map(|o| o.len()).sum::<usize>() * 5 / 2 + bases.len();
    cost.stored = bases.values().map(|b| b.1).sum::<usize>()
        + overrides.values().flat_map(|o| o.values()).sum::<usize>();
    cost
}

fn main() {
    let specs: Vec<String> = std::env::var("MODELS")
        .unwrap_or_else(|_| "all".into())
        .split(',')
        .map(String::from)
        .collect();
    let want = |name: &str| specs.iter().any(|s| s == "all" || name.starts_with(s.as_str()));
    println!("| Trace | Model | B/flush | Rebase B/flush | Rebases | Puts/flush | Map B | Stored KiB | Cold B/chunk |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for path in std::env::args().skip(1) {
        let flushes = parse(&std::fs::read(&path).unwrap());
        let start = flushes.iter().position(|f| f.mark).expect("no mark");
        let mut base = Image::default();
        for flush in &flushes[..start] {
            base.apply(flush);
        }
        let measured: Vec<&Flush> = flushes[start..].iter().collect();
        let n = measured.len() as f64;
        let name = std::path::Path::new(&path).file_stem().unwrap().to_string_lossy();
        let mut models: Vec<(String, Box<dyn Fn() -> Cost>)> = vec![
            ("page-cow".into(), Box::new(|| page_cow(&base, &measured))),
            ("delta-chain".into(), Box::new(|| delta_chain(&base, &measured))),
        ];
        let budget: usize = std::env::var("BUDGET").ok().and_then(|b| b.parse().ok()).unwrap_or(usize::MAX);
        for c in [1u64, 2, 4, 8, 16, 64] {
            for ratio in [0.5, 1.0, 2.0] {
                let (b, m) = (&base, &measured);
                models.push((
                    format!("chunk-delta c={c} r={ratio}"),
                    Box::new(move || chunk_delta(b, m, c, ratio, false)),
                ));
                models.push((
                    format!("chunk-joint c={c} r={ratio}"),
                    Box::new(move || chunk_delta(b, m, c, ratio, true)),
                ));
                models.push((
                    format!("page-base c={c} r={ratio}"),
                    Box::new(move || page_over_base(b, m, c, ratio, budget)),
                ));
            }
        }
        for (model, run) in models {
            if !want(&model) {
                continue;
            }
            let cost = run();
            println!(
                "| {name} | {model} | {:.0} | {:.0} | {} | {:.2} | {} | {:.0} | {} |",
                cost.bytes as f64 / n,
                cost.rebase_bytes as f64 / n,
                cost.rebases,
                cost.puts as f64 / n,
                cost.map,
                cost.stored as f64 / 1024.0,
                cost.cold,
            );
        }
    }
}
