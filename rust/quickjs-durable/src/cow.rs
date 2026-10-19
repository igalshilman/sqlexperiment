//! Golden images, mapped copy-on-write into the VMs that start from them.
//!
//! A VM of a program starts as the program's golden image. Instead of
//! writing the image into each new VM's memory (11 MB for CPython, page by
//! page faulted in), the runtime keeps it in a sealed memory file and maps
//! the file privately over the start of the VM's memory: the kernel shares
//! its pages between every VM of the program, and copies one only when a VM
//! writes to it.
//!
//! Wasmtime does the same for a module's data segments, but takes the image
//! from the module only: one module per program, each compiled again. Here
//! one module per engine (without data segments: its memory starts as
//! zeros) serves every program, and the runtime maps the image itself. The
//! mapping is Wasmtime's memory, so the VM must put fresh zeros back before
//! Wasmtime reuses it ([`unmap`]), as its slot would hold after any VM.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::OnceLock;

/// A program's golden memory, and the sealed file it is mapped from, made on
/// first use.
pub(crate) struct Golden {
    bytes: Vec<u8>,
    file: OnceLock<Result<OwnedFd, String>>,
}

impl Golden {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            file: OnceLock::new(),
        }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn file(&self) -> io::Result<&OwnedFd> {
        self.file
            .get_or_init(|| memory_file(&self.bytes).map_err(|error| error.to_string()))
            .as_ref()
            .map_err(|error| io::Error::other(error.clone()))
    }

    /// Map the image privately over the `len` bytes at `base`, its length.
    ///
    /// # Safety
    ///
    /// `base` is the start of a VM's linear memory, of at least this image's
    /// length, accessible, and nothing else maps or reads it meanwhile.
    pub(crate) unsafe fn map(&self, base: *mut u8) -> io::Result<()> {
        let file = self.file()?;
        let mapped = unsafe {
            libc::mmap(
                base.cast(),
                self.bytes.len(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_FIXED,
                file.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Put fresh zero pages back over `[base, base + len)`, where an image was
/// mapped: the memory Wasmtime gave, as it expects to find it.
///
/// # Safety
///
/// As for [`Golden::map`]; afterwards the memory reads as zeros.
pub(crate) unsafe fn unmap(base: *mut u8, len: usize) -> io::Result<()> {
    let mapped = unsafe {
        libc::mmap(
            base.cast(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_FIXED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A sealed memory file holding `bytes`: no one can change it once made.
fn memory_file(bytes: &[u8]) -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::memfd_create(
            c"golden".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut written = 0;
    while written < bytes.len() {
        let n = unsafe {
            libc::write(
                file.as_raw_fd(),
                bytes[written..].as_ptr().cast(),
                bytes.len() - written,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        written += n as usize;
    }
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}
