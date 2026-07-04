//! Endpoint hot-plug notifications via IMMNotificationClient.
//!
//! Reference:
//! https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nn-mmdeviceapi-immnotificationclient
//!
//! The callbacks arrive on COM worker threads; they only push a marker into
//! a channel, and the GUI thread polls `take_changes` and re-enumerates.

use std::sync::mpsc::{Receiver, Sender, channel};

use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::{PCWSTR, Result, implement};

#[implement(IMMNotificationClient)]
struct NotificationClient {
    tx: Sender<()>,
}

impl NotificationClient {
    fn notify(&self) {
        // The receiver may already be gone during shutdown; ignore.
        let _ = self.tx.send(());
    }
}

impl IMMNotificationClient_Impl for NotificationClient_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> Result<()> {
        self.notify();
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> Result<()> {
        self.notify();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> Result<()> {
        self.notify();
        Ok(())
    }

    fn OnDefaultDeviceChanged(&self, _flow: EDataFlow, _role: ERole, _id: &PCWSTR) -> Result<()> {
        self.notify();
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> Result<()> {
        // Property changes (e.g. renames) do not affect the device set.
        Ok(())
    }
}

/// Registers for endpoint notifications; unregisters on drop. COM must stay
/// initialized on the creating thread for the watcher's lifetime.
pub struct HotplugWatcher {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
    rx: Receiver<()>,
}

impl HotplugWatcher {
    pub fn new() -> Result<Self> {
        let (tx, rx) = channel();
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let client: IMMNotificationClient = NotificationClient { tx }.into();
        unsafe { enumerator.RegisterEndpointNotificationCallback(&client)? };
        Ok(Self {
            enumerator,
            client,
            rx,
        })
    }

    /// Drains pending notifications; true if the device set may have changed
    /// since the last poll.
    pub fn take_changes(&self) -> bool {
        let mut changed = false;
        while self.rx.try_recv().is_ok() {
            changed = true;
        }
        changed
    }
}

impl Drop for HotplugWatcher {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .enumerator
                .UnregisterEndpointNotificationCallback(&self.client);
        }
    }
}
