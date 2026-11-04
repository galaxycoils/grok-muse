pub mod client;
pub mod mock;
pub mod permissions;
pub mod shm;
pub mod traits;

#[cfg(target_os = "macos")]
pub mod mac;

#[cfg(any(target_os = "linux", test))]
pub mod linux;

#[cfg(any(target_os = "windows", test))]
pub mod windows;

pub use client::IpcPlatformClient;
pub use mock::MockPlatform;
pub use permissions::PrivacyPane;
pub use shm::{ShmFrameRing, resolve_frame_bytes, resolve_frame_data};
pub use traits::Platform;

#[cfg(target_os = "macos")]
pub use mac::MacPlatform;

#[cfg(any(target_os = "linux", test))]
pub use linux::LinuxPlatform;

#[cfg(any(target_os = "windows", test))]
pub use windows::WindowsPlatform;
