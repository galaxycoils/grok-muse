pub mod client;
pub mod mock;
pub mod permissions;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod pixels;
pub mod shm;
pub mod traits;

#[cfg(target_os = "macos")]
pub mod mac;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "windows")]
pub mod windows;

pub use client::IpcPlatformClient;
pub use mock::MockPlatform;
pub use permissions::PrivacyPane;
pub use shm::{ShmFrameRing, resolve_frame_bytes, resolve_frame_data};
pub use traits::Platform;

#[cfg(target_os = "macos")]
pub use mac::MacPlatform;

#[cfg(target_os = "linux")]
pub use linux::LinuxPlatform;

#[cfg(target_os = "windows")]
pub use windows::WindowsPlatform;
