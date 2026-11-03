pub mod client;
pub mod mock;
pub mod traits;

pub use client::IpcPlatformClient;
pub use mock::MockPlatform;
pub use traits::Platform;
