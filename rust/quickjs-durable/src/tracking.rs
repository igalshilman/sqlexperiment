//! Native write tracking, on Linux 6.7 and later: the kernel records which
//! pages of a VM's memory were written, at no cost to the code that writes.
//!
//! The memory is registered with a `userfaultfd` in asynchronous
//! write-protect mode: a write to a protected page is resolved in the kernel,
//! which unprotects the page and goes on, with no signal and no thread to
//! wake. `PAGEMAP_SCAN` then lists the pages written since they were last
//! protected, and protects them again, in one call.
//!
//! [`Tracker::new`] fails where the kernel or the sandbox lacks either; the
//! runtime then instruments every store instead (`instrument`).
use std::{fs::File, io, os::fd::AsRawFd, os::fd::FromRawFd, os::fd::OwnedFd};

const PAGE: u64 = 4096;

// <linux/userfaultfd.h>
const UFFD_USER_MODE_ONLY: libc::c_int = 1;
const UFFD_API: u64 = 0xAA;
const UFFD_FEATURE_WP_UNPOPULATED: u64 = 1 << 13;
const UFFD_FEATURE_WP_ASYNC: u64 = 1 << 15;
const UFFDIO_REGISTER_MODE_WP: u64 = 1 << 1;

#[repr(C)]
struct UffdioApi {
    api: u64,
    features: u64,
    ioctls: u64,
}
#[repr(C)]
struct UffdioRange {
    start: u64,
    len: u64,
}
#[repr(C)]
struct UffdioRegister {
    range: UffdioRange,
    mode: u64,
    ioctls: u64,
}
// <linux/fs.h>
const PM_SCAN_WP_MATCHING: u64 = 1 << 0;
const PM_SCAN_CHECK_WPASYNC: u64 = 1 << 1;
const PAGE_IS_WRITTEN: u64 = 1 << 1;

#[repr(C)]
struct PmScanArg {
    size: u64,
    flags: u64,
    start: u64,
    end: u64,
    walk_end: u64,
    vec: u64,
    vec_len: u64,
    max_pages: u64,
    category_inverted: u64,
    category_mask: u64,
    category_anyof_mask: u64,
    return_mask: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PageRegion {
    start: u64,
    end: u64,
    categories: u64,
}

/// The process's write tracker: one `userfaultfd` for every VM's memory.
pub(crate) struct Tracker {
    uffd: OwnedFd,
    pagemap: File,
}

impl Tracker {
    pub(crate) fn new() -> io::Result<Self> {
        let fd = unsafe {
            libc::syscall(
                libc::SYS_userfaultfd,
                libc::O_CLOEXEC | libc::O_NONBLOCK | UFFD_USER_MODE_ONLY,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let uffd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let mut api = UffdioApi {
            api: UFFD_API,
            features: UFFD_FEATURE_WP_ASYNC | UFFD_FEATURE_WP_UNPOPULATED,
            ioctls: 0,
        };
        ioctl(&uffd, libc::_IOWR::<UffdioApi>(0xAA, 0x3F), &mut api)?;
        let pagemap = File::open("/proc/self/pagemap")?;
        Ok(Self { uffd, pagemap })
    }

    /// Track writes to `[start, start + len)`, page-aligned. Registering a
    /// range already registered here again is harmless.
    pub(crate) fn register(&self, start: usize, len: usize) -> io::Result<()> {
        let mut register = UffdioRegister {
            range: range(start, len),
            mode: UFFDIO_REGISTER_MODE_WP,
            ioctls: 0,
        };
        ioctl(
            &self.uffd,
            libc::_IOWR::<UffdioRegister>(0xAA, 0x00),
            &mut register,
        )
    }

    /// The indexes of the range's pages written since protected, in order;
    /// with `protect`, protected again in the same walk.
    pub(crate) fn written(
        &self,
        start: usize,
        len: usize,
        protect: bool,
    ) -> io::Result<Vec<usize>> {
        let mut pages = Vec::new();
        let mut regions = [PageRegion::default(); 64];
        let end = (start + len) as u64;
        let mut from = start as u64;
        while from < end {
            let mut arg = PmScanArg {
                size: size_of::<PmScanArg>() as u64,
                flags: PM_SCAN_CHECK_WPASYNC | if protect { PM_SCAN_WP_MATCHING } else { 0 },
                start: from,
                end,
                walk_end: 0,
                vec: regions.as_mut_ptr() as u64,
                vec_len: regions.len() as u64,
                max_pages: 0,
                category_inverted: 0,
                category_mask: PAGE_IS_WRITTEN,
                category_anyof_mask: 0,
                return_mask: PAGE_IS_WRITTEN,
            };
            let found = unsafe {
                libc::ioctl(
                    self.pagemap.as_raw_fd(),
                    libc::_IOWR::<PmScanArg>(b'f' as u32, 16),
                    &mut arg,
                )
            };
            if found < 0 {
                return Err(io::Error::last_os_error());
            }
            for region in &regions[..found as usize] {
                let first = (region.start - start as u64) / PAGE;
                let last = (region.end - start as u64) / PAGE;
                pages.extend(first as usize..last as usize);
            }
            if arg.walk_end <= from {
                break;
            }
            from = arg.walk_end;
        }
        Ok(pages)
    }
}

fn range(start: usize, len: usize) -> UffdioRange {
    UffdioRange {
        start: start as u64,
        len: len as u64,
    }
}

fn ioctl<T>(fd: &OwnedFd, request: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    if unsafe { libc::ioctl(fd.as_raw_fd(), request, arg as *mut T) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
