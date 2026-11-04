use gm_core::error::{GmError, Result};
use gm_core::types::{FrameCapture, FrameData};
#[cfg(unix)]
use parking_lot::Mutex;
#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
static RING_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// Default capacity per shared-memory ring slot (8 MiB virtual pages; only touched pages fault).
pub const DEFAULT_SHM_SLOT_CAPACITY: usize = 8 * 1024 * 1024;
#[cfg(unix)]
const RING_SLOTS: usize = 4;

#[cfg(unix)]
struct ShmSlot {
    shm_id: String,
    ptr: *mut u8,
    capacity: usize,
}

#[cfg(unix)]
// SAFETY: Access to `ShmSlot::ptr` is serialized per slot via `Mutex` inside `ShmFrameRing`.
unsafe impl Send for ShmSlot {}
#[cfg(unix)]
unsafe impl Sync for ShmSlot {}

#[cfg(unix)]
impl Drop for ShmSlot {
    fn drop(&mut self) {
        unsafe {
            if !self.ptr.is_null() && self.ptr != libc::MAP_FAILED.cast::<u8>() {
                let _ = libc::munmap(self.ptr.cast::<libc::c_void>(), self.capacity);
            }
            if let Ok(c_name) = CString::new(self.shm_id.as_str()) {
                let _ = libc::shm_unlink(c_name.as_ptr());
            }
        }
    }
}

/// Persistent multi-slot POSIX shared-memory ring for zero-copy IPC frame transfer.
///
/// Uses short `<31` byte names (required by macOS XNU `PSHMNAMLEN`) with `0o600` permissions,
/// sizing each slot once via `ftruncate` at initialization to satisfy Darwin's single-`ftruncate`
/// invariant on POSIX `shm` segments.
pub struct ShmFrameRing {
    #[cfg(unix)]
    slots: Vec<Mutex<ShmSlot>>,
    #[cfg(unix)]
    next_slot: AtomicU64,
    #[cfg(not(unix))]
    _capacity: usize,
}

impl Default for ShmFrameRing {
    fn default() -> Self {
        Self::new(DEFAULT_SHM_SLOT_CAPACITY)
    }
}

impl ShmFrameRing {
    pub fn new(slot_capacity: usize) -> Self {
        #[cfg(unix)]
        {
            let cap = slot_capacity.max(4096);
            let pid = std::process::id();
            let inst = RING_INSTANCE_ID.fetch_add(1, Ordering::Relaxed) % 10_000;
            let mut slots = Vec::with_capacity(RING_SLOTS);

            for idx in 0..RING_SLOTS {
                // Keep name strictly <= 30 chars for macOS PSHMNAMLEN
                let shm_id = format!("/gm_{pid}_{inst}_{idx}");
                if let Some(slot) = Self::create_slot(&shm_id, cap) {
                    slots.push(Mutex::new(slot));
                }
            }

            Self {
                slots,
                next_slot: AtomicU64::new(0),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                _capacity: slot_capacity,
            }
        }
    }

    #[cfg(unix)]
    fn create_slot(shm_id: &str, capacity: usize) -> Option<ShmSlot> {
        let c_name = CString::new(shm_id).ok()?;
        unsafe {
            // Unlink any stale segment with the same name first
            let _ = libc::shm_unlink(c_name.as_ptr());

            #[cfg(target_os = "macos")]
            let fd = libc::shm_open(
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600 as libc::c_uint,
            );
            #[cfg(not(target_os = "macos"))]
            let fd = libc::shm_open(
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600 as libc::mode_t,
            );

            if fd < 0 {
                return None;
            }

            if libc::ftruncate(fd, capacity as libc::off_t) != 0 {
                let _ = libc::close(fd);
                let _ = libc::shm_unlink(c_name.as_ptr());
                return None;
            }

            let mapped = libc::mmap(
                std::ptr::null_mut(),
                capacity,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            let _ = libc::close(fd);

            if mapped == libc::MAP_FAILED || mapped.is_null() {
                let _ = libc::shm_unlink(c_name.as_ptr());
                return None;
            }

            Some(ShmSlot {
                shm_id: shm_id.to_string(),
                ptr: mapped.cast::<u8>(),
                capacity,
            })
        }
    }

    /// Writes `bytes` into the next ring slot and returns `FrameData::SharedMemory`,
    /// falling back to `FrameData::InlineBytes` if empty, oversized, or on non-Unix.
    pub fn store_bytes(&self, bytes: Vec<u8>) -> FrameData {
        #[cfg(unix)]
        {
            if bytes.is_empty() || self.slots.is_empty() {
                return FrameData::InlineBytes(bytes);
            }
            let slot_idx =
                (self.next_slot.fetch_add(1, Ordering::Relaxed) as usize) % self.slots.len();
            let slot = self.slots[slot_idx].lock();
            if bytes.len() > slot.capacity {
                return FrameData::InlineBytes(bytes);
            }
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), slot.ptr, bytes.len());
            }
            FrameData::SharedMemory {
                shm_id: slot.shm_id.clone(),
                size_bytes: bytes.len(),
            }
        }
        #[cfg(not(unix))]
        {
            FrameData::InlineBytes(bytes)
        }
    }

    /// Promotes an `InlineBytes` `FrameCapture` into `SharedMemory` when possible.
    pub fn promote_frame(&self, mut frame: FrameCapture) -> FrameCapture {
        if let FrameData::InlineBytes(bytes) = frame.data {
            frame.data = self.store_bytes(bytes);
        }
        frame
    }
}

/// Resolves any `FrameData` variant (`InlineBytes`, `SharedMemory`, `FilePath`) into raw bytes.
pub fn resolve_frame_data(data: &FrameData) -> Result<Vec<u8>> {
    match data {
        FrameData::InlineBytes(bytes) => Ok(bytes.clone()),
        FrameData::FilePath(path) => std::fs::read(path).map_err(GmError::Io),
        FrameData::SharedMemory { shm_id, size_bytes } => read_shm_segment(shm_id, *size_bytes),
    }
}

/// Convenience helper resolving `FrameCapture.data` into bytes.
pub fn resolve_frame_bytes(frame: &FrameCapture) -> Result<Vec<u8>> {
    resolve_frame_data(&frame.data)
}

#[cfg(unix)]
fn read_shm_segment(shm_id: &str, size_bytes: usize) -> Result<Vec<u8>> {
    if size_bytes == 0 {
        return Ok(Vec::new());
    }
    let c_name = CString::new(shm_id).map_err(|e| {
        GmError::Ipc(format!(
            "Invalid shared memory segment name '{shm_id}': {e}"
        ))
    })?;

    unsafe {
        #[cfg(target_os = "macos")]
        let fd = libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0 as libc::c_uint);
        #[cfg(not(target_os = "macos"))]
        let fd = libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0 as libc::mode_t);

        if fd < 0 {
            let err = std::io::Error::last_os_error();
            return Err(GmError::Ipc(format!(
                "Failed to open POSIX shared memory '{shm_id}': {err}"
            )));
        }

        let mapped = libc::mmap(
            std::ptr::null_mut(),
            size_bytes,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        );
        let _ = libc::close(fd);

        if mapped == libc::MAP_FAILED || mapped.is_null() {
            let err = std::io::Error::last_os_error();
            return Err(GmError::Ipc(format!(
                "Failed to mmap POSIX shared memory '{shm_id}' ({size_bytes} bytes): {err}"
            )));
        }

        let slice = std::slice::from_raw_parts(mapped.cast::<u8>(), size_bytes);
        let out = slice.to_vec();
        let _ = libc::munmap(mapped, size_bytes);
        Ok(out)
    }
}

#[cfg(not(unix))]
fn read_shm_segment(shm_id: &str, _size_bytes: usize) -> Result<Vec<u8>> {
    Err(GmError::Platform(format!(
        "POSIX shared memory ('{shm_id}') is not supported on this OS"
    )))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn test_shm_frame_ring_roundtrip_wraparound_and_drop_cleanup() {
        let shm_name_recorded: String;
        {
            let ring = ShmFrameRing::new(64 * 1024);

            // Write 6 frames across the 4-slot ring to test wrap-around without Darwin ftruncate issues
            let mut last_data = None;
            for i in 0..6u8 {
                let payload: Vec<u8> = (0..4096).map(|k| (k as u8).wrapping_add(i)).collect();
                let fd = ring.store_bytes(payload.clone());
                match &fd {
                    FrameData::SharedMemory { shm_id, size_bytes } => {
                        assert!(shm_id.len() < 31, "macOS PSHMNAMLEN requires <31 chars");
                        assert_eq!(*size_bytes, payload.len());
                    }
                    other => panic!("Expected SharedMemory, got {other:?}"),
                }
                let recovered = resolve_frame_data(&fd).expect("must read shared memory");
                assert_eq!(recovered, payload);
                last_data = Some(fd);
            }

            // Oversized payload gracefully falls back to InlineBytes
            let huge = vec![0xABu8; 80 * 1024];
            let fallback = ring.store_bytes(huge.clone());
            assert!(matches!(fallback, FrameData::InlineBytes(_)));
            assert_eq!(resolve_frame_data(&fallback).unwrap(), huge);

            if let Some(FrameData::SharedMemory { shm_id, .. }) = last_data {
                shm_name_recorded = shm_id;
            } else {
                panic!("Missing recorded shm_id");
            }
        }

        // After ShmFrameRing is dropped, the kernel segment must be unlinked
        let after_drop = read_shm_segment(&shm_name_recorded, 16);
        assert!(
            after_drop.is_err(),
            "Shared memory segment must be unlinked on Drop"
        );
    }
}
