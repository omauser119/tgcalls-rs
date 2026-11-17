//! Platform policies and the no-JIT Wasm interpreter used by iOS Hardened Mode.
pub mod wasm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    LinuxSeccomp,
    AndroidIsolatedService,
    MacosXpc,
    IosInterpreter,
    Unsupported,
}
impl Strategy {
    pub const fn current() -> Self {
        #[cfg(target_os = "android")]
        {
            return Self::AndroidIsolatedService;
        }
        #[cfg(target_os = "macos")]
        {
            return Self::MacosXpc;
        }
        #[cfg(target_os = "ios")]
        {
            return Self::IosInterpreter;
        }
        #[cfg(target_os = "linux")]
        {
            Self::LinuxSeccomp
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        )))]
        {
            Self::Unsupported
        }
    }
}
