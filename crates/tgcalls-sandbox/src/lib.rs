//! Linux worker isolation and supervision.
#[cfg(not(target_os = "linux"))]
compile_error!("The hardened worker currently requires Linux");

pub mod ipc;
pub mod policy;
pub mod protocol;
pub mod ring;
pub mod supervisor;
pub mod worker;

pub use protocol::Config;
pub use supervisor::Sandbox;
