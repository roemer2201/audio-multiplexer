//! Minimal RAII wrapper for COM initialization.

use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::Result;

/// Initializes COM (MTA) on the current thread and uninitializes it on drop.
///
/// Every thread that calls into Core Audio APIs must hold one of these for
/// its whole lifetime. See:
/// https://learn.microsoft.com/en-us/windows/win32/api/combaseapi/nf-combaseapi-coinitializeex
pub struct ComGuard(());

impl ComGuard {
    pub fn new() -> Result<Self> {
        // S_FALSE (already initialized) is also a success code; ok() accepts it.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Self(()))
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
