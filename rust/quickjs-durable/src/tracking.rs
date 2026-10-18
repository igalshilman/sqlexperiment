//! Optional Linux kernel capability probe. The active checkpoint implementation
//! uses portable Wasm write barriers and does not depend on this probe. Results
//! describe this process/container, not merely the kernel version from uname.
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Capability {
    pub available: bool,
    pub reason: String,
}

/// Probe Linux async userfaultfd write protection without registering or changing
/// any memory. Production use must also verify registration of its actual mapping.
pub fn probe() -> Capability {
    #[cfg(target_os = "linux")]
    {
        #[repr(C)]
        struct Api {
            api: u64,
            features: u64,
            ioctls: u64,
        }
        // UFFD_USER_MODE_ONLY avoids requiring kernel-fault handling privileges.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_userfaultfd,
                libc::O_CLOEXEC | libc::O_NONBLOCK | 1,
            )
        };
        if fd < 0 {
            return Capability {
                available: false,
                reason: format!("userfaultfd: {}", std::io::Error::last_os_error()),
            };
        }
        let mut api = Api {
            api: 0xAA,
            features: 0,
            ioctls: 0,
        };
        // Architecture-specific ioctl encoding with the 24-byte uffdio_api ABI.
        let result = unsafe { libc::ioctl(fd as i32, libc::_IOWR::<Api>(0xAA, 0x3F), &mut api) };
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(fd as i32) };
        if result < 0 {
            return Capability {
                available: false,
                reason: format!("UFFDIO_API: {error}"),
            };
        }
        let required = (1 << 15) | (1 << 13);
        let available = api.features & required == required;
        Capability {
            available,
            reason: format!(
                "UFFD feature mask: 0x{:x}; async WP + unpopulated pages {}",
                api.features,
                if available {
                    "supported"
                } else {
                    "unavailable"
                }
            ),
        }
    }
    #[cfg(not(target_os = "linux"))]
    Capability {
        available: false,
        reason: "Linux userfaultfd is unavailable on this platform".into(),
    }
}
