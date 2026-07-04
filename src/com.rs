//! Minimal RAII wrapper for COM initialization.

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::Result;

/// Initializes COM (MTA) on the current thread and uninitializes it on drop.
///
/// Every thread that calls into Core Audio APIs must hold one of these for
/// its whole lifetime. See:
/// https://learn.microsoft.com/en-us/windows/win32/api/combaseapi/nf-combaseapi-coinitializeex
///
/// If the thread already has COM initialized in a different apartment mode
/// (RPC_E_CHANGED_MODE; the GUI event loop initializes STA for drag and
/// drop), that existing initialization is reused: Core Audio works from an
/// STA as well, and we must not uninitialize what we did not initialize.
pub struct ComGuard {
    uninit_on_drop: bool,
}

impl ComGuard {
    pub fn new() -> Result<Self> {
        // S_FALSE (already initialized in the same mode) is a success code
        // and adds a reference that must be released on drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Self {
                uninit_on_drop: false,
            });
        }
        hr.ok()?;
        Ok(Self {
            uninit_on_drop: true,
        })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit_on_drop {
            unsafe { CoUninitialize() };
        }
    }
}
