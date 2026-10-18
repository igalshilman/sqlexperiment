use crate::tracking::Tracker;

/// An anonymous mapping of `pages` pages, unmapped when dropped.
struct Mapping(*mut u8, usize);
impl Mapping {
    fn new(pages: usize) -> Self {
        let len = pages * 4096;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        Self(ptr.cast(), len)
    }
    fn write(&self, page: usize) {
        unsafe { self.0.add(page * 4096 + 7).write_volatile(1) };
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.0.cast(), self.1) };
    }
}

#[test]
fn the_kernel_lists_the_pages_written_since_protected() {
    let Ok(tracker) = Tracker::new() else {
        eprintln!("No native write tracking here");
        return;
    };
    let memory = Mapping::new(64);
    let (start, len) = (memory.0 as usize, memory.1);
    memory.write(0);
    tracker.register(start, len).unwrap();
    tracker.register(start, len).unwrap();
    tracker.written(start, len, true).unwrap();
    assert_eq!(
        tracker.written(start, len, true).unwrap(),
        Vec::<usize>::new()
    );
    for page in [3, 10, 11, 63] {
        memory.write(page);
    }
    // Reading an untouched page is not a write.
    let _ = unsafe { memory.0.add(20 * 4096).read_volatile() };
    assert_eq!(tracker.written(start, len, false).unwrap(), [3, 10, 11, 63]);
    assert_eq!(tracker.written(start, len, true).unwrap(), [3, 10, 11, 63]);
    assert_eq!(
        tracker.written(start, len, true).unwrap(),
        Vec::<usize>::new()
    );
    memory.write(11);
    assert_eq!(tracker.written(start, len, true).unwrap(), [11]);
}
