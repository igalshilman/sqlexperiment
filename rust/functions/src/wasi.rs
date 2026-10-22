//! The WASI a Python VM sees: a read-only filesystem holding the standard
//! library, stdout and stderr into the host's log, the turn's clock, and
//! nothing else. CPython imports modules through it; programs get no other I/O.
//!
//! Every file reports the same times, and inodes number the files in order,
//! so evaluating a program reads the same metadata in every process and its
//! golden image is the same byte for byte. Open files live here, outside the
//! VM's memory, so none may stay open across a wait ([`Fds::idle`]).
use crate::vm::{Host, caller_memory, caller_write};
use std::{collections::BTreeMap, path::Path, sync::Arc};
use wasmtime::error::Context;
use wasmtime::{Caller, Linker, Result, bail, ensure};

const SUCCESS: i32 = 0;
const EBADF: i32 = 8;
const EINVAL: i32 = 28;
const EISDIR: i32 = 31;
const ENOENT: i32 = 44;
const ENOSYS: i32 = 52;
const ENOTDIR: i32 = 54;
const ENOTSUP: i32 = 58;
const EROFS: i32 = 69;
const ESPIPE: i32 = 70;

const CHARACTER_DEVICE: u8 = 2;
const DIRECTORY: u8 = 3;
const REGULAR_FILE: u8 = 4;

/// The one preopened directory, `/`.
const ROOT: u32 = 3;
const FD_WRITE: u64 = 1 << 6;
const O_CREAT: i32 = 1;
const O_DIRECTORY: i32 = 2;
const O_EXCL: i32 = 4;
const O_TRUNC: i32 = 8;

enum Entry {
    File(Arc<[u8]>),
    /// Its children's names, in order.
    Dir(Vec<String>),
}

/// A read-only tree of files, by path relative to `/` ("" is the root).
pub(crate) struct Filesystem {
    /// Each entry, with its inode: its place in the tree's order.
    entries: BTreeMap<String, (Entry, u64)>,
}

impl Filesystem {
    /// Every file under `dir`, read now, mounted at `at` (say `lib/python3.15`).
    pub(crate) fn load(dir: &Path, at: &str) -> Result<Self> {
        let mut files = BTreeMap::new();
        let mut pending = vec![(dir.to_path_buf(), at.to_string())];
        while let Some((dir, path)) = pending.pop() {
            for entry in std::fs::read_dir(&dir).with_context(|| format!("{}", dir.display()))? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .ok()
                    .context("Non-UTF-8 name")?;
                let child = format!("{path}/{name}");
                if entry.file_type()?.is_dir() {
                    pending.push((entry.path(), child));
                } else {
                    files.insert(child, Arc::from(std::fs::read(entry.path())?));
                }
            }
        }
        Ok(Self::new(files))
    }

    fn new(files: BTreeMap<String, Arc<[u8]>>) -> Self {
        let mut entries = BTreeMap::from([(String::new(), Entry::Dir(Vec::new()))]);
        for (path, bytes) in files {
            let mut parent = String::new();
            for name in path.split('/') {
                let child = if parent.is_empty() {
                    name.to_string()
                } else {
                    format!("{parent}/{name}")
                };
                if !entries.contains_key(&child) {
                    if let Some(Entry::Dir(children)) = entries.get_mut(&parent) {
                        children.push(name.to_string());
                    }
                    entries.insert(child.clone(), Entry::Dir(Vec::new()));
                }
                parent = child;
            }
            entries.insert(path, Entry::File(bytes));
        }
        let entries = entries
            .into_iter()
            .enumerate()
            .map(|(index, (path, mut entry))| {
                if let Entry::Dir(children) = &mut entry {
                    children.sort();
                }
                (path, (entry, index as u64 + 1))
            })
            .collect();
        Self { entries }
    }

    /// The entry at `path`, and its inode.
    fn get(&self, path: &str) -> Option<(&Entry, u64)> {
        self.entries.get(path).map(|(entry, inode)| (entry, *inode))
    }
}

/// `path` resolved against `base`, without `.` and `..`; `None` above the root.
fn resolve(base: &str, path: &str) -> Option<String> {
    let mut parts: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        base.split('/').filter(|part| !part.is_empty()).collect()
    };
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

struct Open {
    path: String,
    offset: u64,
}

/// The guest's open files and directories, from fd 4 on.
#[derive(Default)]
pub(crate) struct Fds {
    open: BTreeMap<u32, Open>,
}

impl Fds {
    /// Nothing open but stdio and the root: the VM's memory is all its state.
    pub(crate) fn idle(&self) -> Result<()> {
        ensure!(
            self.open.is_empty(),
            "A file is open across a wait: close it (`with open(...)`) before awaiting"
        );
        Ok(())
    }

    fn insert(&mut self, path: String) -> u32 {
        let fd = (ROOT + 1..).find(|fd| !self.open.contains_key(fd)).unwrap();
        self.open.insert(fd, Open { path, offset: 0 });
        fd
    }

    fn path(&self, fd: u32) -> Option<&str> {
        if fd == ROOT {
            return Some("");
        }
        self.open.get(&fd).map(|open| open.path.as_str())
    }
}

fn filesystem<'a>(c: &'a Caller<'_, Host>) -> Result<&'a Filesystem> {
    c.data().filesystem.as_deref().context("No filesystem")
}

fn read_bytes(c: &mut Caller<'_, Host>, ptr: i32, len: i32) -> Result<Vec<u8>> {
    let mut bytes = vec![0; len as u32 as usize];
    caller_memory(c)?.read(&*c, ptr as u32 as usize, &mut bytes)?;
    Ok(bytes)
}

fn read_path(c: &mut Caller<'_, Host>, fd: i32, ptr: i32, len: i32) -> Result<Option<String>> {
    let path = String::from_utf8(read_bytes(c, ptr, len)?)?;
    let Some(base) = c.data().fds.path(fd as u32) else {
        return Ok(None);
    };
    Ok(resolve(base, &path))
}

/// A `filestat`: device, inode, type, links, size and three times, all 0.
fn filestat(entry: &Entry, inode: u64) -> [u8; 64] {
    let mut stat = [0; 64];
    stat[8..16].copy_from_slice(&inode.to_le_bytes());
    let (kind, size) = match entry {
        Entry::File(bytes) => (REGULAR_FILE, bytes.len() as u64),
        Entry::Dir(_) => (DIRECTORY, 0),
    };
    stat[16] = kind;
    stat[24..32].copy_from_slice(&1_u64.to_le_bytes());
    stat[32..40].copy_from_slice(&size.to_le_bytes());
    stat
}

/// The I/O vectors at `iovs`: (address, length) pairs.
fn vectors(c: &mut Caller<'_, Host>, iovs: i32, count: i32) -> Result<Vec<(usize, usize)>> {
    let count = count as u32 as usize;
    ensure!(count <= 1024, "Too many I/O vectors");
    let raw = read_bytes(c, iovs, (count * 8) as i32)?;
    Ok(raw
        .chunks_exact(8)
        .map(|pair| {
            let at = u32::from_le_bytes(pair[..4].try_into().unwrap()) as usize;
            let len = u32::from_le_bytes(pair[4..].try_into().unwrap()) as usize;
            (at, len)
        })
        .collect())
}

/// Copy `bytes` from `offset` into the vectors; the count copied.
fn scatter(
    c: &mut Caller<'_, Host>,
    iovs: i32,
    count: i32,
    bytes: &[u8],
    offset: u64,
) -> Result<usize> {
    let mut from = (offset as usize).min(bytes.len());
    let start = from;
    for (at, len) in vectors(c, iovs, count)? {
        let n = len.min(bytes.len() - from);
        caller_write(c, at, &bytes[from..from + n])?;
        from += n;
    }
    Ok(from - start)
}

/// An open file's bytes and offset, or the errno that refuses reading it.
type Readable = std::result::Result<(Arc<[u8]>, u64), i32>;

fn file_bytes(c: &Caller<'_, Host>, fd: u32) -> Result<Readable> {
    let Some(open) = c.data().fds.open.get(&fd) else {
        return Ok(Err(if fd <= 2 { ESPIPE } else { EBADF }));
    };
    Ok(match filesystem(c)?.get(&open.path) {
        Some((Entry::File(bytes), _)) => Ok((bytes.clone(), open.offset)),
        Some((Entry::Dir(_), _)) => Err(EISDIR),
        None => Err(EBADF),
    })
}

fn write_u32(c: &mut Caller<'_, Host>, ptr: i32, value: u32) -> Result<()> {
    caller_write(c, ptr as u32 as usize, &value.to_le_bytes())
}

fn write_u64(c: &mut Caller<'_, Host>, ptr: i32, value: u64) -> Result<()> {
    caller_write(c, ptr as u32 as usize, &value.to_le_bytes())
}

/// Define `wasi_snapshot_preview1` for CPython. Clocks and randomness are the
/// VM's (see [`crate::vm`]); this module adds the rest.
pub(crate) fn link(linker: &mut Linker<Host>) -> Result<()> {
    const WASI: &str = "wasi_snapshot_preview1";
    linker.func_wrap(
        WASI,
        "environ_sizes_get",
        |mut c: Caller<'_, Host>, count: i32, size: i32| {
            write_u32(&mut c, count, 0)?;
            write_u32(&mut c, size, 0)?;
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(WASI, "environ_get", |_: i32, _: i32| SUCCESS)?;
    linker.func_wrap(
        WASI,
        "clock_res_get",
        |mut c: Caller<'_, Host>, _clock: i32, ptr: i32| {
            write_u64(&mut c, ptr, 1_000_000)?;
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(WASI, "proc_exit", |code: i32| -> Result<()> {
        bail!("Python exited with status {code}")
    })?;
    linker.func_wrap(WASI, "sched_yield", || SUCCESS)?;
    linker.func_wrap(WASI, "fd_advise", |_: i32, _: i64, _: i64, _: i32| SUCCESS)?;
    linker.func_wrap(
        WASI,
        "fd_prestat_get",
        |mut c: Caller<'_, Host>, fd: i32, ptr: i32| {
            if fd as u32 != ROOT {
                return Ok(EBADF);
            }
            caller_write(&mut c, ptr as u32 as usize, &[0, 0, 0, 0, 1, 0, 0, 0])?;
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(
        WASI,
        "fd_prestat_dir_name",
        |mut c: Caller<'_, Host>, fd: i32, ptr: i32, len: i32| {
            if fd as u32 != ROOT || len < 1 {
                return Ok(EBADF);
            }
            caller_write(&mut c, ptr as u32 as usize, b"/")?;
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(
        WASI,
        "fd_fdstat_get",
        |mut c: Caller<'_, Host>, fd: i32, ptr: i32| {
            let fd = fd as u32;
            let kind = match fd {
                1 | 2 => CHARACTER_DEVICE,
                _ => match c.data().fds.path(fd).map(str::to_string) {
                    None => return Ok(EBADF),
                    Some(path) => match filesystem(&c)?.get(&path) {
                        Some((Entry::File(_), _)) => REGULAR_FILE,
                        Some((Entry::Dir(_), _)) => DIRECTORY,
                        None => return Ok(EBADF),
                    },
                },
            };
            let mut stat = [0_u8; 24];
            stat[0] = kind;
            // Every right but writing.
            stat[8..16].copy_from_slice(&(!FD_WRITE).to_le_bytes());
            stat[16..24].copy_from_slice(&(!FD_WRITE).to_le_bytes());
            if kind == CHARACTER_DEVICE {
                stat[8..16].copy_from_slice(&FD_WRITE.to_le_bytes());
            }
            caller_write(&mut c, ptr as u32 as usize, &stat)?;
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(
        WASI,
        "fd_filestat_get",
        |mut c: Caller<'_, Host>, fd: i32, ptr: i32| {
            let fd = fd as u32;
            let stat = if fd == 1 || fd == 2 {
                let mut stat = [0; 64];
                stat[16] = CHARACTER_DEVICE;
                stat
            } else {
                let Some(path) = c.data().fds.path(fd).map(str::to_string) else {
                    return Ok(EBADF);
                };
                let Some((entry, inode)) = filesystem(&c)?.get(&path) else {
                    return Ok(EBADF);
                };
                filestat(entry, inode)
            };
            caller_write(&mut c, ptr as u32 as usize, &stat)?;
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(
        WASI,
        "path_filestat_get",
        |mut c: Caller<'_, Host>, fd: i32, _flags: i32, path: i32, len: i32, ptr: i32| {
            let Some(path) = read_path(&mut c, fd, path, len)? else {
                return Ok(ENOENT);
            };
            let Some((entry, inode)) = filesystem(&c)?.get(&path) else {
                return Ok(ENOENT);
            };
            let stat = filestat(entry, inode);
            caller_write(&mut c, ptr as u32 as usize, &stat)?;
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(
        WASI,
        "path_open",
        |mut c: Caller<'_, Host>,
         fd: i32,
         _dirflags: i32,
         path: i32,
         len: i32,
         oflags: i32,
         rights: i64,
         _inheriting: i64,
         _fdflags: i32,
         out: i32| {
            let Some(path) = read_path(&mut c, fd, path, len)? else {
                return Ok(ENOENT);
            };
            let is_dir = match filesystem(&c)?.get(&path) {
                None if oflags & O_CREAT != 0 => return Ok(EROFS),
                None => return Ok(ENOENT),
                Some((Entry::Dir(_), _)) => true,
                Some((Entry::File(_), _)) => false,
            };
            if oflags & (O_EXCL | O_TRUNC) != 0 || rights as u64 & FD_WRITE != 0 {
                return Ok(EROFS);
            }
            if oflags & O_DIRECTORY != 0 && !is_dir {
                return Ok(ENOTDIR);
            }
            let fd = c.data_mut().fds.insert(path);
            write_u32(&mut c, out, fd)?;
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(WASI, "fd_close", |mut c: Caller<'_, Host>, fd: i32| {
        Ok::<_, wasmtime::Error>(if c.data_mut().fds.open.remove(&(fd as u32)).is_some() {
            SUCCESS
        } else {
            EBADF
        })
    })?;
    linker.func_wrap(
        WASI,
        "fd_read",
        |mut c: Caller<'_, Host>, fd: i32, iovs: i32, count: i32, nread: i32| {
            if fd == 0 {
                write_u32(&mut c, nread, 0)?;
                return Ok(SUCCESS);
            }
            let (bytes, offset) = match file_bytes(&c, fd as u32)? {
                Ok(file) => file,
                Err(errno) => return Ok(errno),
            };
            let n = scatter(&mut c, iovs, count, &bytes, offset)?;
            c.data_mut().fds.open.get_mut(&(fd as u32)).unwrap().offset += n as u64;
            write_u32(&mut c, nread, n as u32)?;
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(
        WASI,
        "fd_pread",
        |mut c: Caller<'_, Host>, fd: i32, iovs: i32, count: i32, offset: i64, nread: i32| {
            let (bytes, _) = match file_bytes(&c, fd as u32)? {
                Ok(file) => file,
                Err(errno) => return Ok(errno),
            };
            let n = scatter(&mut c, iovs, count, &bytes, offset as u64)?;
            write_u32(&mut c, nread, n as u32)?;
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(
        WASI,
        "fd_seek",
        |mut c: Caller<'_, Host>, fd: i32, delta: i64, whence: i32, out: i32| {
            let (bytes, offset) = match file_bytes(&c, fd as u32)? {
                Ok(file) => file,
                Err(errno) => return Ok(errno),
            };
            let from = match whence {
                0 => 0,
                1 => offset as i64,
                2 => bytes.len() as i64,
                _ => return Ok(EINVAL),
            };
            let Some(next) = from.checked_add(delta).filter(|next| *next >= 0) else {
                return Ok(EINVAL);
            };
            c.data_mut().fds.open.get_mut(&(fd as u32)).unwrap().offset = next as u64;
            write_u64(&mut c, out, next as u64)?;
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(
        WASI,
        "fd_tell",
        |mut c: Caller<'_, Host>, fd: i32, out: i32| {
            let (_, offset) = match file_bytes(&c, fd as u32)? {
                Ok(file) => file,
                Err(errno) => return Ok(errno),
            };
            write_u64(&mut c, out, offset)?;
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(
        WASI,
        "fd_readdir",
        |mut c: Caller<'_, Host>, fd: i32, buf: i32, len: i32, cookie: i64, used: i32| {
            let Some(path) = c.data().fds.path(fd as u32).map(str::to_string) else {
                return Ok(EBADF);
            };
            let files = filesystem(&c)?;
            let Some((Entry::Dir(children), _)) = files.get(&path) else {
                return Ok(ENOTDIR);
            };
            // Each entry: next cookie, inode, name length, type, then the name.
            let mut out = Vec::new();
            for (index, name) in children.iter().enumerate().skip(cookie as usize) {
                let child = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}/{name}")
                };
                let (entry, inode) = files.get(&child).context("Missing child")?;
                out.extend_from_slice(&(index as u64 + 1).to_le_bytes());
                out.extend_from_slice(&inode.to_le_bytes());
                out.extend_from_slice(&(name.len() as u32).to_le_bytes());
                out.push(if matches!(entry, Entry::Dir(_)) {
                    DIRECTORY
                } else {
                    REGULAR_FILE
                });
                out.extend_from_slice(&[0; 3]);
                out.extend_from_slice(name.as_bytes());
                if out.len() >= len as u32 as usize {
                    break;
                }
            }
            // A truncated last entry tells libc to retry with a larger buffer.
            out.truncate(len as u32 as usize);
            caller_write(&mut c, buf as u32 as usize, &out)?;
            write_u32(&mut c, used, out.len() as u32)?;
            Ok(SUCCESS)
        },
    )?;
    linker.func_wrap(
        WASI,
        "path_readlink",
        |mut c: Caller<'_, Host>,
         fd: i32,
         path: i32,
         len: i32,
         _buf: i32,
         _buf_len: i32,
         _used: i32| {
            Ok(match read_path(&mut c, fd, path, len)? {
                Some(path) if filesystem(&c)?.get(&path).is_some() => EINVAL,
                _ => ENOENT,
            })
        },
    )?;
    linker.func_wrap(
        WASI,
        "fd_write",
        |mut c: Caller<'_, Host>, fd: i32, iovs: i32, count: i32, written: i32| {
            if fd != 1 && fd != 2 {
                return Ok(EBADF);
            }
            let mut text = Vec::new();
            for (at, len) in vectors(&mut c, iovs, count)? {
                text.extend(read_bytes(&mut c, at as i32, len as i32)?);
            }
            // The program's prints, in the host's log.
            use std::io::Write;
            let _ = std::io::stderr().write_all(&text);
            write_u32(&mut c, written, text.len() as u32)?;
            Ok(SUCCESS)
        },
    )?;
    // Writing, sockets and polling are not there: each fails with its errno.
    use wasmtime::ValType::{I32, I64};
    let unsupported: [(&str, &[wasmtime::ValType], i32); 18] = [
        ("fd_datasync", &[I32], EBADF),
        ("fd_sync", &[I32], EBADF),
        ("fd_fdstat_set_flags", &[I32, I32], ENOTSUP),
        ("fd_filestat_set_size", &[I32, I64], EROFS),
        ("fd_filestat_set_times", &[I32, I64, I64, I32], EROFS),
        ("fd_pwrite", &[I32, I32, I32, I64, I32], EBADF),
        ("path_create_directory", &[I32, I32, I32], EROFS),
        (
            "path_filestat_set_times",
            &[I32, I32, I32, I32, I64, I64, I32],
            EROFS,
        ),
        ("path_link", &[I32, I32, I32, I32, I32, I32, I32], EROFS),
        ("path_remove_directory", &[I32, I32, I32], EROFS),
        ("path_rename", &[I32, I32, I32, I32, I32, I32], EROFS),
        ("path_symlink", &[I32, I32, I32, I32, I32], EROFS),
        ("path_unlink_file", &[I32, I32, I32], EROFS),
        ("poll_oneoff", &[I32, I32, I32, I32], ENOSYS),
        ("sock_accept", &[I32, I32, I32], ENOTSUP),
        ("sock_recv", &[I32, I32, I32, I32, I32, I32], ENOTSUP),
        ("sock_send", &[I32, I32, I32, I32, I32], ENOTSUP),
        ("sock_shutdown", &[I32, I32], ENOTSUP),
    ];
    for (name, params, errno) in unsupported {
        let ty = wasmtime::FuncType::new(linker.engine(), params.iter().cloned(), [I32]);
        linker.func_new(WASI, name, ty, move |_, _, results| {
            results[0] = wasmtime::Val::I32(errno);
            Ok(())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn paths_resolve_inside_the_root() {
        assert_eq!(
            resolve("", "lib/python3.15/os.py").as_deref(),
            Some("lib/python3.15/os.py")
        );
        assert_eq!(resolve("lib", "./a/../b").as_deref(), Some("lib/b"));
        assert_eq!(resolve("lib/x", "/abs").as_deref(), Some("abs"));
        assert_eq!(resolve("", ".."), None);
    }
}
