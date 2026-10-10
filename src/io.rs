//! Byte-addressed SQLite engine files over on-demand 4 KiB KV blocks.
//!
//! All writes are private until atomic snapshot publication. sync() is an
//! overlay barrier; Session::save is the backing-store durability boundary.
use crate::kv::KvStore;
use crate::snapshot::{Snapshot, StorageOptions};
use futures::channel::mpsc;
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use turso_core::io::{FileId, FileSyncType, clock::DefaultClock};
use turso_core::{
    Buffer, Clock, Completion, CompletionError, File, IO, LimboError, MonotonicInstant, OpenFlags,
    WallClockInstant,
};

pub const BLOCK_SIZE: usize = 4096;
pub const DB_PATH: &str = "object.db";
pub const WAL_PATH: &str = "object.db-wal";
const FILES: [&str; 2] = [DB_PATH, WAL_PATH];

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
fn engine_error(message: impl Into<String>) -> LimboError {
    LimboError::InternalError(message.into())
}
#[derive(Default)]
struct State {
    sizes: [u64; 2],
    dirty: BTreeMap<(usize, u64), Option<Vec<u8>>>,
    #[cfg(feature = "simulation")]
    written: [u64; 2],
}

enum Operation {
    Read {
        file: usize,
        pos: u64,
    },
    Write {
        file: usize,
        pos: u64,
        bytes: Vec<u8>,
    },
    Truncate {
        file: usize,
        len: u64,
    },
    Sync,
}
struct Pending {
    op: Operation,
    completion: Completion,
}

#[derive(Default)]
struct Progress {
    generation: u64,
    inflight: usize,
    stopped: bool,
}

struct Completing<'a> {
    io: &'a KvIo,
    completion: Completion,
    transferred: bool,
}
impl Drop for Completing<'_> {
    fn drop(&mut self) {
        if self.transferred {
            return;
        }
        if !self.completion.finished() {
            self.completion.abort();
        }
        let mut progress = self.io.progress.lock().unwrap();
        progress.inflight -= 1;
        progress.generation += 1;
        self.io.wake.notify_all();
    }
}

pub struct KvIo {
    snapshot: Snapshot,
    state: Mutex<State>,
    queue: Mutex<VecDeque<Pending>>,
    driver: futures::lock::Mutex<()>,
    worker_completions: AtomicBool,
    ready: Mutex<VecDeque<(Completion, i32)>>,
    events: mpsc::UnboundedSender<()>,
    event_receiver: Mutex<Option<mpsc::UnboundedReceiver<()>>>,
    progress: Mutex<Progress>,
    wake: Condvar,
    #[cfg(all(test, feature = "simulation"))]
    fault: Mutex<Option<test_faults::Fault>>,
}

impl KvIo {
    /// Recover a pending save, load the head, then fetch nodes/blocks on demand.
    pub async fn mount(store: Arc<dyn KvStore>) -> io::Result<Arc<Self>> {
        Self::mount_with_options(store, StorageOptions::default()).await
    }

    pub async fn mount_with_options(
        store: Arc<dyn KvStore>,
        options: StorageOptions,
    ) -> io::Result<Arc<Self>> {
        let snapshot = Snapshot::open_with_options(store, options).await?;
        let state = State {
            sizes: [snapshot.size, 0],
            ..State::default()
        };
        let (events, event_receiver) = mpsc::unbounded();
        Ok(Arc::new(Self {
            snapshot,
            state: Mutex::new(state),
            queue: Mutex::default(),
            driver: futures::lock::Mutex::new(()),
            worker_completions: AtomicBool::new(false),
            ready: Mutex::default(),
            events,
            event_receiver: Mutex::new(Some(event_receiver)),
            progress: Mutex::default(),
            wake: Condvar::new(),
            #[cfg(all(test, feature = "simulation"))]
            fault: Mutex::new(None),
        }))
    }

    /// Engine success callbacks run only from IO::step on the engine worker.
    /// Standalone drivers can complete directly; Session selects worker delivery.
    pub(crate) fn use_worker_completions(&self) {
        self.worker_completions.store(true, Ordering::SeqCst);
    }

    #[cfg(feature = "simulation")]
    pub fn stats(&self) -> crate::simulation::IoStats {
        let state = self.state.lock().unwrap();
        crate::simulation::IoStats {
            database_bytes_written: state.written[0],
            private_wal_bytes_written: state.written[1],
            database_len: state.sizes[0],
            private_wal_len: state.sizes[1],
        }
    }

    pub(crate) fn wal_len(&self) -> u64 {
        self.state.lock().unwrap().sizes[1]
    }

    fn enqueue(&self, op: Operation, completion: Completion) -> turso_core::Result<Completion> {
        let mut progress = self.progress.lock().unwrap();
        if progress.stopped {
            return Err(engine_error("adapter stopped"));
        }
        progress.inflight += 1;
        self.queue.lock().unwrap().push_back(Pending {
            op,
            completion: completion.clone(),
        });
        let _ = self.events.unbounded_send(());
        Ok(completion)
    }

    pub(crate) fn take_events(&self) -> mpsc::UnboundedReceiver<()> {
        self.event_receiver
            .lock()
            .unwrap()
            .take()
            .expect("one host driver")
    }

    pub(crate) fn shutdown(&self) {
        {
            let mut progress = self.progress.lock().unwrap();
            progress.stopped = true;
            self.wake.notify_all();
        }
        let pending: Vec<_> = self.queue.lock().unwrap().drain(..).collect();
        for request in pending {
            drop(Completing {
                io: self,
                completion: request.completion,
                transferred: false,
            });
        }
        let ready: Vec<_> = self.ready.lock().unwrap().drain(..).collect();
        for (completion, _) in ready {
            drop(Completing {
                io: self,
                completion,
                transferred: false,
            });
        }
    }

    pub fn pending_count(&self) -> usize {
        self.progress.lock().unwrap().inflight
    }

    async fn block(&self, file: usize, index: u64) -> io::Result<Vec<u8>> {
        let staged = self
            .state
            .lock()
            .unwrap()
            .dirty
            .get(&(file, index))
            .cloned();
        let block = match staged {
            Some(value) => value,
            None if file == 0 => self.snapshot.block(index).await?,
            None => None, // WAL is always session-local; never fetch it from KV
        };
        match block {
            Some(bytes) if bytes.len() == BLOCK_SIZE => Ok(bytes),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid block length",
            )),
            None => Ok(vec![0; BLOCK_SIZE]), // sparse-file hole
        }
    }

    async fn read(&self, file: usize, pos: u64, output: &mut [u8]) -> io::Result<usize> {
        output.fill(0);
        let size = self.state.lock().unwrap().sizes[file];
        let count = size.saturating_sub(pos).min(output.len() as u64) as usize;
        let mut done = 0;
        while done < count {
            let offset = pos + done as u64;
            let index = offset / BLOCK_SIZE as u64;
            let start = (offset % BLOCK_SIZE as u64) as usize;
            let len = (BLOCK_SIZE - start).min(count - done);
            let block = self.block(file, index).await?;
            output[done..done + len].copy_from_slice(&block[start..start + len]);
            done += len;
        }
        Ok(count)
    }

    async fn write(&self, file: usize, pos: u64, bytes: &[u8]) -> io::Result<usize> {
        let end = pos
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("offset overflow"))?;
        let mut done = 0;
        while done < bytes.len() {
            let offset = pos + done as u64;
            let index = offset / BLOCK_SIZE as u64;
            let start = (offset % BLOCK_SIZE as u64) as usize;
            let len = (BLOCK_SIZE - start).min(bytes.len() - done);
            // Full blocks do not require a read before overwrite.
            let mut block = if start == 0 && len == BLOCK_SIZE {
                vec![0; BLOCK_SIZE]
            } else {
                self.block(file, index).await?
            };
            block[start..start + len].copy_from_slice(&bytes[done..done + len]);
            self.state
                .lock()
                .unwrap()
                .dirty
                .insert((file, index), Some(block));
            done += len;
        }
        if !bytes.is_empty() {
            let mut state = self.state.lock().unwrap();
            state.sizes[file] = state.sizes[file].max(end);
            #[cfg(feature = "simulation")]
            {
                state.written[file] += bytes.len() as u64;
            }
        }
        Ok(bytes.len())
    }

    async fn truncate_file(&self, file: usize, len: u64) -> io::Result<()> {
        let old_size = self.state.lock().unwrap().sizes[file];
        if len < old_size {
            let tail = (len % BLOCK_SIZE as u64) as usize;
            if tail != 0 {
                let index = len / BLOCK_SIZE as u64;
                let mut block = self.block(file, index).await?;
                block[tail..].fill(0);
                self.state
                    .lock()
                    .unwrap()
                    .dirty
                    .insert((file, index), Some(block));
            }
            let first = len.div_ceil(BLOCK_SIZE as u64);
            let last = old_size.div_ceil(BLOCK_SIZE as u64);
            let mut state = self.state.lock().unwrap();
            for index in first..last {
                state.dirty.insert((file, index), None);
            }
        }
        self.state.lock().unwrap().sizes[file] = len;
        Ok(())
    }

    /// Host-owned async I/O pump. No backend access happens in File::pread itself.
    pub async fn drive(&self) -> io::Result<usize> {
        // Keep submission order across awaits even if low-level callers drive
        // the adapter concurrently. Session normally has only one caller.
        let _driver = self.driver.lock().await;
        let mut completed = 0;
        loop {
            let pending = self.queue.lock().unwrap().pop_front();
            let Some(Pending { op, completion }) = pending else {
                break;
            };
            let mut finishing = Completing {
                io: self,
                completion: completion.clone(),
                transferred: false,
            };
            if completion.finished() {
                continue;
            }
            #[cfg(feature = "simulation")]
            crate::simulation::yield_once().await;
            #[cfg(all(test, feature = "simulation"))]
            self.fault_point(false).await?;
            let result = match op {
                Operation::Read { file, pos } => {
                    // Keep the engine's buffer untouched across await and cancellation.
                    let mut bytes = vec![0; completion.as_read().buf().len()];
                    match self.read(file, pos, &mut bytes).await {
                        Ok(n) => {
                            completion
                                .as_read()
                                .buf()
                                .as_mut_slice()
                                .copy_from_slice(&bytes);
                            Ok(n)
                        }
                        Err(e) => Err(e),
                    }
                }
                Operation::Write { file, pos, bytes } => self.write(file, pos, &bytes).await,
                Operation::Truncate { file, len } => self.truncate_file(file, len).await.map(|_| 0),
                Operation::Sync => Ok(0),
            };
            #[cfg(all(test, feature = "simulation"))]
            self.fault_point(true).await?;
            match result {
                Ok(n) => {
                    let n = i32::try_from(n).map_err(|_| invalid("I/O too large"))?;
                    if self.worker_completions.load(Ordering::SeqCst) {
                        let mut progress = self.progress.lock().unwrap();
                        if !progress.stopped {
                            self.ready
                                .lock()
                                .unwrap()
                                .push_back((completion.clone(), n));
                            finishing.transferred = true;
                            progress.generation += 1;
                            self.wake.notify_all();
                        }
                    } else {
                        completion.complete(n);
                    }
                }
                Err(e) => {
                    completion.error(CompletionError::IOError(e.kind(), "KV I/O"));
                    return Err(e);
                }
            }
            completed += 1;
        }
        Ok(completed)
    }

    /// Publish after the engine has stopped and no SQL transaction remains.
    pub(crate) async fn publish(&self) -> io::Result<crate::SaveReport> {
        if self.pending_count() != 0 {
            return Err(invalid("cannot save with pending I/O"));
        }
        let (dirty, size) = {
            let state = self.state.lock().unwrap();
            if state.sizes[1] != 0 {
                return Err(invalid(
                    "WAL must be fully checkpointed and truncated before publication",
                ));
            }
            let dirty = state
                .dirty
                .iter()
                .filter(|((file, _), _)| *file == 0)
                .map(|((_, block), bytes)| (*block, bytes.clone()))
                .collect();
            (dirty, state.sizes[0])
        };
        self.snapshot.publish(dirty, size).await
    }
}

impl Clock for KvIo {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        DefaultClock.current_time_monotonic()
    }
    fn current_time_wall_clock(&self) -> WallClockInstant {
        DefaultClock.current_time_wall_clock()
    }
}

impl IO for KvIo {
    fn open_file(&self, _: &str, _: OpenFlags, _: bool) -> turso_core::Result<Arc<dyn File>> {
        Err(engine_error(
            "use MountedIo: file handles need the shared adapter",
        ))
    }
    fn remove_file(&self, _: &str) -> turso_core::Result<()> {
        Err(engine_error("dynamic files are out of scope"))
    }
    fn step(&self) -> turso_core::Result<()> {
        if self.worker_completions.load(Ordering::SeqCst) {
            loop {
                let ready: Vec<_> = self.ready.lock().unwrap().drain(..).collect();
                if !ready.is_empty() {
                    for (completion, n) in ready {
                        let _finishing = Completing {
                            io: self,
                            completion: completion.clone(),
                            transferred: false,
                        };
                        completion.complete(n);
                    }
                    return Ok(());
                }
                let mut progress = self.progress.lock().unwrap();
                if progress.stopped {
                    return Err(engine_error("adapter stopped"));
                }
                if progress.inflight == 0 {
                    return Ok(());
                }
                // Both scheduling and this check take progress before ready,
                // preventing a missed wake between the empty check and wait.
                if self.ready.lock().unwrap().is_empty() {
                    progress = self.wake.wait(progress).unwrap();
                }
                drop(progress);
            }
        }
        // Only the dedicated engine thread blocks here. The host completes I/O.
        let mut progress = self.progress.lock().unwrap();
        let generation = progress.generation;
        while progress.inflight > 0 && progress.generation == generation && !progress.stopped {
            progress = self.wake.wait(progress).unwrap();
        }
        if progress.stopped {
            return Err(engine_error("adapter stopped"));
        }
        Ok(())
    }
    fn file_id(&self, path: &str) -> turso_core::Result<FileId> {
        Ok(FileId::from_path_hash(path))
    }
}

/// Thin Arc holder, avoiding self-referential backend/file ownership.
pub struct MountedIo(pub Arc<KvIo>);
impl Clock for MountedIo {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        self.0.current_time_monotonic()
    }
    fn current_time_wall_clock(&self) -> WallClockInstant {
        self.0.current_time_wall_clock()
    }
}
impl IO for MountedIo {
    fn open_file(&self, path: &str, _: OpenFlags, _: bool) -> turso_core::Result<Arc<dyn File>> {
        let file = FILES
            .iter()
            .position(|name| *name == path)
            .ok_or_else(|| engine_error(format!("unsupported file: {path}")))?;
        Ok(Arc::new(KvFile {
            io: self.0.clone(),
            file,
        }))
    }
    fn remove_file(&self, path: &str) -> turso_core::Result<()> {
        self.0.remove_file(path)
    }
    fn step(&self) -> turso_core::Result<()> {
        self.0.step()
    }
    fn file_id(&self, path: &str) -> turso_core::Result<FileId> {
        self.0.file_id(path)
    }
}

struct KvFile {
    io: Arc<KvIo>,
    file: usize,
}
impl File for KvFile {
    // One owner/connection per mounted store is a host-enforced precondition.
    fn lock_file(&self, _: bool) -> turso_core::Result<()> {
        Ok(())
    }
    fn unlock_file(&self) -> turso_core::Result<()> {
        Ok(())
    }
    fn pread(&self, pos: u64, c: Completion) -> turso_core::Result<Completion> {
        self.io.enqueue(
            Operation::Read {
                file: self.file,
                pos,
            },
            c,
        )
    }
    fn pwrite(
        &self,
        pos: u64,
        buffer: Arc<Buffer>,
        c: Completion,
    ) -> turso_core::Result<Completion> {
        self.io.enqueue(
            Operation::Write {
                file: self.file,
                pos,
                bytes: buffer.as_slice().to_vec(),
            },
            c,
        )
    }
    fn pwritev(
        &self,
        pos: u64,
        buffers: Vec<Arc<Buffer>>,
        c: Completion,
    ) -> turso_core::Result<Completion> {
        // One queued operation/completion: do not use the upstream default's
        // child-completion aggregation, which complicates error propagation.
        let bytes = buffers
            .iter()
            .flat_map(|b| b.as_slice().iter().copied())
            .collect();
        self.io.enqueue(
            Operation::Write {
                file: self.file,
                pos,
                bytes,
            },
            c,
        )
    }
    fn sync(&self, c: Completion, _: FileSyncType) -> turso_core::Result<Completion> {
        self.io.enqueue(Operation::Sync, c)
    }
    fn size(&self) -> turso_core::Result<u64> {
        Ok(self.io.state.lock().unwrap().sizes[self.file])
    }
    fn truncate(&self, len: u64, c: Completion) -> turso_core::Result<Completion> {
        self.io.enqueue(
            Operation::Truncate {
                file: self.file,
                len,
            },
            c,
        )
    }
}

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/io.rs"]
mod completion_tests;

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/io_faults.rs"]
mod test_faults;
