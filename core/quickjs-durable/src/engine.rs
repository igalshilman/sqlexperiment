use crate::{Transaction, codec, model::*, vm::Runtime};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc};

const FORMAT: u32 = 1;
const MAX_JSON: usize = 16 * 1024 * 1024;

/// Runs within a host-owned transaction. All writes and effect requests remain
/// staged until the host commits; a returned View is not itself a commit.
/// The host serializes ownership, commits or discards the whole transaction,
/// and chooses the explicit recovery/delivery path. There is no replay log.
/// Discard the engine and transaction after error or cancellation of a turn.
/// Never commit a transaction whose start/complete future did not finish.
pub struct Engine<K: Transaction> {
    kv: K,
    runtime: Arc<Runtime>,
    checkpoint_interval: Option<NonZeroUsize>,
    usable: bool,
}
struct Loaded {
    id: String,
    manifest: Manifest,
    image: Image,
    manifest_bytes: usize,
}
struct Golden {
    application: Application,
    /// Shared by every turn of the application's runs: see `Runtime::golden_memory`.
    memory: Arc<Vec<u8>>,
}
impl Golden {
    fn image(&self) -> Image {
        Image {
            meta: self.application.meta.clone(),
            memory: self.memory.to_vec(),
        }
    }
}

fn run_prefix(run: &str) -> String {
    format!("run/{}/", hash(run.as_bytes()))
}
fn app_key(id: &str) -> String {
    format!("application/{}", hash(id.as_bytes()))
}
fn blob_key(id: &str) -> String {
    format!("blob/{id}")
}
fn checkpoint_key(id: &str) -> String {
    format!("checkpoint/{id}")
}
fn identifier(id: &str) -> Result<()> {
    ensure!(!id.is_empty() && id.len() <= 1024, "Invalid identifier");
    Ok(())
}
fn input_size<T: serde::Serialize>(input: &T) -> Result<()> {
    ensure!(json_bytes(input)?.len() <= MAX_INPUT, "Input exceeds limit");
    Ok(())
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    ensure!(bytes.len() <= MAX_JSON, "Record exceeds limit");
    Ok(serde_json::from_slice(bytes)?)
}
fn page(memory: &[u8], index: usize) -> &[u8] {
    static ZERO: [u8; PAGE] = [0; PAGE];
    memory
        .get(index * PAGE..(index + 1) * PAGE)
        .unwrap_or(&ZERO)
}

impl<K: Transaction> Engine<K> {
    pub fn new(kv: K, runtime: Arc<Runtime>) -> Self {
        Self {
            kv,
            runtime,
            checkpoint_interval: None,
            usable: true,
        }
    }

    /// Host scheduling policy: capture after at least this many Ready requests,
    /// at the end of a request batch, before running their queued continuations.
    /// The host commits an internal Resume wakeup with the image. This is not
    /// preemption of a running JS job or pending transactional I/O. Without this
    /// policy, capture only when waiting for durable effects or finishing.
    pub fn with_checkpoint_interval(mut self, interval: NonZeroUsize) -> Self {
        self.checkpoint_interval = Some(interval);
        self
    }
    pub fn storage(&self) -> &K {
        &self.kv
    }
    /// Extract only a complete transaction. An error or a cancelled mutating
    /// future permanently poisons this engine; consuming it then drops storage.
    pub fn into_storage(self) -> Result<K> {
        ensure!(
            self.usable,
            "Unfinished or failed engine transaction; discard it"
        );
        Ok(self.kv)
    }

    fn begin_mutation(&mut self) -> Result<()> {
        ensure!(
            self.usable,
            "Unfinished or failed engine transaction; discard it"
        );
        // Set before any mutation/await. A dropped future never resets this.
        self.usable = false;
        Ok(())
    }

    /// Stage a golden image once, before invocation-specific state is bound.
    /// Reusing an ID with different source or a different build is rejected.
    pub fn register(&mut self, id: &str, source: &str) -> Result<()> {
        self.begin_mutation()?;
        let result = self.register_inner(id, source);
        self.usable = result.is_ok();
        result
    }

    fn register_inner(&mut self, id: &str, source: &str) -> Result<()> {
        identifier(id)?;
        if self.kv.get(&app_key(id))?.is_some() {
            let golden = self.golden(id)?;
            ensure!(
                golden.application.source_hash == hash(source.as_bytes()),
                "Application version is immutable"
            );
            return Ok(());
        }
        let image = self.runtime.golden(id, source)?;
        let bytes = codec::compress(&image.memory)?;
        let blob = app_key(id);
        self.put_blob(&blob, &bytes)?;
        let application = Application {
            format: FORMAT,
            id: id.into(),
            source_hash: hash(source.as_bytes()),
            build: self.runtime.build.clone(),
            blob,
            memory_len: image.memory.len(),
            meta: image.meta,
        };
        self.kv.put(&app_key(id), &json_bytes(&application)?)
    }

    /// Stage the initial checkpoint, local KV mutations and outgoing effects.
    /// A retry after an ambiguous host commit returns the already-staged/committed
    /// head if application and input agree. The input record is identity metadata;
    /// it is never replayed by recovery.
    pub async fn start(
        &mut self,
        run: &str,
        application: &str,
        input: Value,
        now: u64,
    ) -> Result<View> {
        self.begin_mutation()?;
        let result = self.start_inner(run, application, input, now).await;
        self.usable = result.is_ok();
        result
    }

    async fn start_inner(
        &mut self,
        run: &str,
        application: &str,
        input: Value,
        now: u64,
    ) -> Result<View> {
        identifier(run)?;
        identifier(application)?;
        self.kv.check_run(run)?;
        validate_time(now)?;
        input_size(&input)?;
        let key = format!("{}input", run_prefix(run));
        if let Some(current) = self.load(run)? {
            let original: Value = decode(&self.kv.get(&key)?.context("Missing invocation input")?)?;
            ensure!(
                current.manifest.application == application && original == input,
                "Run already belongs to another application/input"
            );
            return Ok(Self::view(current));
        }
        self.kv.put(&key, &json_bytes(&input)?)?;
        let event = Event::Start {
            application_id: application.into(),
            input,
            now,
        };
        Ok(Self::view(self.checkpoint(run, None, &event).await?))
    }

    /// The host delivers a result through its explicit recovery/inbox path.
    /// Consumption of that result, application state, new effects and the next
    /// VM checkpoint all belong to the same transaction. No event is journaled.
    pub async fn complete(
        &mut self,
        run: &str,
        operation: &str,
        outcome: Outcome,
        now: u64,
    ) -> Result<View> {
        self.begin_mutation()?;
        let result = self.complete_inner(run, operation, outcome, now).await;
        self.usable = result.is_ok();
        result
    }

    async fn complete_inner(
        &mut self,
        run: &str,
        operation: &str,
        outcome: Outcome,
        now: u64,
    ) -> Result<View> {
        identifier(run)?;
        self.kv.check_run(run)?;
        validate_time(now)?;
        outcome.validate()?;
        input_size(&outcome)?;
        let current = self.load(run)?.context("Run does not exist")?;
        let pending = current
            .manifest
            .state
            .pending
            .iter()
            .find(|p| p.id() == operation);
        let Some(pending) = pending else {
            let number = operation
                .strip_prefix(&format!("{run}/"))
                .and_then(|s| s.parse::<u64>().ok());
            ensure!(
                number.is_some_and(|n| n < current.manifest.state.next_operation
                    && operation == format!("{run}/{n}")),
                "Unknown operation ID"
            );
            return Ok(Self::view(current));
        };
        let now = now.max(current.manifest.state.now);
        if let Operation::Sleep { deadline, .. } = pending {
            ensure!(now >= *deadline, "Timer completed before deadline");
        }
        self.kv.acknowledge(operation, &outcome, now)?;
        let event = Event::Complete {
            operation_id: operation.into(),
            outcome,
            now,
        };
        Ok(Self::view(
            self.checkpoint(run, Some(current), &event).await?,
        ))
    }

    /// Read only: restore the current snapshot and pending-effect identities.
    /// No guest execution, host callback, effect re-emission or event replay.
    /// The host reconnects the pending IDs to its own durable effect state.
    pub fn recover(&self, run: &str) -> Result<View> {
        ensure!(
            self.usable,
            "Unfinished or failed engine transaction; discard it"
        );
        identifier(run)?;
        self.kv.check_run(run)?;
        Ok(Self::view(self.load(run)?.context("Run does not exist")?))
    }

    fn view(loaded: Loaded) -> View {
        let m = loaded.manifest;
        View {
            checkpoint_id: loaded.id,
            application_id: m.application,
            checkpoint_sequence: m.sequence,
            state: m.state,
            image_bytes: m.memory_len,
            changed_pages: m.changed_pages,
            examined_pages: m.examined_pages,
            override_pages: m.pages.len(),
            new_blob_bytes: m.new_blob_bytes,
            manifest_bytes: loaded.manifest_bytes,
        }
    }
    fn get_blob(&self, id: &str) -> Result<Vec<u8>> {
        let bytes = self.kv.get(&blob_key(id))?.context("Missing blob")?;
        ensure!(
            bytes.len() <= MAX_MEMORY + 1024 * 1024,
            "Blob exceeds size limit"
        );
        Ok(bytes)
    }
    fn put_blob(&mut self, id: &str, bytes: &[u8]) -> Result<()> {
        self.kv.put(&blob_key(id), bytes)
    }
    fn golden(&self, id: &str) -> Result<Golden> {
        let application: Application =
            decode(&self.kv.get(&app_key(id))?.context("Missing application")?)?;
        ensure!(
            application.format == FORMAT
                && application.id == id
                && application.build == self.runtime.build,
            "Incompatible application build/format"
        );
        ensure!(
            application.memory_len <= MAX_MEMORY && application.memory_len.is_multiple_of(65536),
            "Invalid golden memory size"
        );
        let memory = self.runtime.golden_memory(&application, || {
            let memory =
                codec::decompress(&self.get_blob(&application.blob)?, application.memory_len)?;
            ensure!(
                memory.len() == application.memory_len,
                "Golden image size mismatch"
            );
            Ok(memory)
        })?;
        Ok(Golden {
            application,
            memory,
        })
    }
    fn load(&self, run: &str) -> Result<Option<Loaded>> {
        let Some(head) = self.kv.get(&format!("{}head", run_prefix(run)))? else {
            return Ok(None);
        };
        let id = String::from_utf8(head)?;
        let bytes = self
            .kv
            .get(&checkpoint_key(&id))?
            .context("Missing checkpoint")?;
        let manifest: Manifest = decode(&bytes)?;
        ensure!(
            manifest.format == FORMAT && manifest.build == self.runtime.build,
            "Incompatible checkpoint build/format"
        );
        let golden = self.golden(&manifest.application)?;
        ensure!(
            manifest.memory_len >= golden.memory.len()
                && manifest.memory_len <= MAX_MEMORY
                && manifest.memory_len.is_multiple_of(65536),
            "Invalid checkpoint memory size"
        );
        let mut memory = golden.memory.to_vec();
        memory.resize(manifest.memory_len, 0);
        // Group references, then release each decoded blob before loading the
        // next. Keeping all historical batches can vastly exceed VM memory.
        let mut blobs: BTreeMap<&str, Vec<(u32, usize)>> = BTreeMap::new();
        for (index, reference) in &manifest.pages {
            ensure!(
                (*index as usize) < memory.len() / PAGE,
                "Page beyond current memory"
            );
            blobs
                .entry(&reference.blob)
                .or_default()
                .push((*index, reference.slot));
        }
        for (blob, references) in blobs {
            let decoded = codec::decode_pages(&self.get_blob(blob)?)?;
            for (index, slot) in references {
                let (stored_index, xor) = decoded.get(slot).context("Invalid page slot")?;
                ensure!(*stored_index == index, "Page reference mismatch");
                let target = &mut memory[index as usize * PAGE..(index as usize + 1) * PAGE];
                for (dst, delta) in target.iter_mut().zip(xor) {
                    *dst ^= delta;
                }
            }
        }
        let image = Image {
            meta: manifest.meta.clone(),
            memory,
        };
        Ok(Some(Loaded {
            id,
            manifest,
            image,
            manifest_bytes: bytes.len(),
        }))
    }

    async fn checkpoint(
        &mut self,
        run: &str,
        mut current: Option<Loaded>,
        event: &Event,
    ) -> Result<Loaded> {
        let sequence = current.as_ref().map_or(0, |c| c.manifest.sequence + 1);
        let id = format!("{}{sequence}", run_prefix(run));
        let application = match (&current, event) {
            (None, Event::Start { application_id, .. }) => application_id.clone(),
            (Some(c), Event::Complete { .. }) => c.manifest.application.clone(),
            _ => anyhow::bail!("Invalid delivery"),
        };
        let golden = self.golden(&application)?;
        let first = current.is_none().then(|| golden.image());
        let base = current
            .as_ref()
            .map_or_else(|| first.as_ref().unwrap(), |c| &c.image);
        let pending = current
            .as_ref()
            .map_or(&[][..], |c| c.manifest.state.pending.as_slice());
        let (capture, state) = self
            .runtime
            .turn(
                run,
                base,
                event,
                pending,
                &mut self.kv,
                self.checkpoint_interval,
            )
            .await?;
        let mut pages = current
            .as_ref()
            .map_or_else(BTreeMap::new, |c| c.manifest.pages.clone());
        // Reuse the reconstructed parent buffer. Capture copies only changed
        // pages out of Wasm; no second full snapshot copy is needed.
        let mut memory = match (current.as_mut(), first) {
            (Some(c), _) => std::mem::take(&mut c.image.memory),
            (None, first) => first.context("Missing golden image")?.memory,
        };
        memory.resize(capture.memory_len, 0);
        let changed = capture.pages.len();
        let mut deltas = Vec::new();
        for (index, new) in capture.pages {
            let original = page(&golden.memory, index as usize);
            memory[index as usize * PAGE..(index as usize + 1) * PAGE].copy_from_slice(&new);
            if original == new {
                pages.remove(&index);
            } else {
                deltas.push((
                    index,
                    new.iter().zip(original).map(|(n, o)| n ^ o).collect(),
                ));
            }
        }
        let next = Image {
            meta: capture.meta,
            memory,
        };
        let mut new_blob_bytes = 0;
        if !deltas.is_empty() {
            let bytes = codec::encode_pages(&deltas)?;
            new_blob_bytes = bytes.len();
            let blob = id.clone();
            self.put_blob(&blob, &bytes)?;
            for (slot, (index, _)) in deltas.iter().enumerate() {
                pages.insert(
                    *index,
                    PageRef {
                        blob: blob.clone(),
                        slot,
                    },
                );
            }
        }
        let manifest = Manifest {
            format: FORMAT,
            application,
            build: self.runtime.build.clone(),
            previous: current.as_ref().map(|c| c.id.clone()),
            sequence,
            meta: next.meta.clone(),
            memory_len: next.memory.len(),
            pages,
            state,
            changed_pages: changed,
            examined_pages: capture.examined_pages,
            new_blob_bytes,
        };
        let bytes = json_bytes(&manifest)?;
        ensure!(bytes.len() <= MAX_JSON, "Checkpoint metadata exceeds limit");
        self.kv.put(&checkpoint_key(&id), &bytes)?;
        // Head is staged with every other mutation. Only the outer host commit
        // makes it visible together with application state and effects.
        self.kv
            .put(&format!("{}head", run_prefix(run)), id.as_bytes())?;
        Ok(Loaded {
            id,
            manifest,
            image: next,
            manifest_bytes: bytes.len(),
        })
    }
}
