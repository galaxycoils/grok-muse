pub mod client;
pub mod mock;
pub mod traits;

#[cfg(target_os = "macos")]
pub mod mac;

pub use client::IpcPlatformClient;
pub use mock::MockPlatform;
pub use traits::Platform;

#[cfg(target_os = "macos")]
pub use mac::MacPlatform;
