//! Enumeration and activation of audio render endpoints.
//!
//! References:
//! - https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nn-mmdeviceapi-immdeviceenumerator
//! - https://learn.microsoft.com/en-us/windows/win32/coreaudio/device-properties
//! - https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-getmixformat

use std::ffi::c_void;
use std::fmt;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::core::{HSTRING, Result};

// Format tag values from mmreg.h. Defined locally to avoid pulling in the
// whole multimedia feature set for three constants.
const WAVE_FORMAT_TAG_PCM: u16 = 0x0001;
const WAVE_FORMAT_TAG_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_TAG_EXTENSIBLE: u16 = 0xFFFE;

// Data1 fields of KSDATAFORMAT_SUBTYPE_PCM / _IEEE_FLOAT. The remaining GUID
// fields are the fixed media-type suffix 0000-0010-8000-00aa00389b71.
const SUBTYPE_DATA1_PCM: u32 = 0x0000_0001;
const SUBTYPE_DATA1_IEEE_FLOAT: u32 = 0x0000_0003;

#[derive(Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub mix_format: MixFormat,
}

#[derive(Clone, Copy)]
pub struct MixFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub sample_type: SampleType,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SampleType {
    Pcm,
    Float,
    Other(u16),
}

/// The sample layouts the streaming engine can convert from and to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StreamSampleKind {
    Float32,
    Pcm16,
}

impl MixFormat {
    /// Returns the supported streaming layout, or None if the engine cannot
    /// handle this mix format yet.
    pub fn stream_kind(&self) -> Option<StreamSampleKind> {
        match (self.sample_type, self.bits_per_sample) {
            (SampleType::Float, 32) => Some(StreamSampleKind::Float32),
            (SampleType::Pcm, 16) => Some(StreamSampleKind::Pcm16),
            _ => None,
        }
    }
}

impl fmt::Display for MixFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sample_type = match self.sample_type {
            SampleType::Pcm => "int".to_string(),
            SampleType::Float => "float".to_string(),
            SampleType::Other(tag) => format!("unknown (tag 0x{tag:04X})"),
        };
        write!(
            f,
            "{} Hz, {} ch, {}-bit {}",
            self.sample_rate, self.channels, self.bits_per_sample, sample_type
        )
    }
}

unsafe fn create_enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// Lists all active render endpoints, including their shared-mode mix format.
///
/// COM must already be initialized on the calling thread (see `com::ComGuard`).
pub fn list_render_devices() -> Result<Vec<DeviceInfo>> {
    unsafe {
        let enumerator = create_enumerator()?;

        // The default endpoint is only used to mark the matching list entry;
        // there may be none at all (no active devices), so errors are ignored.
        let default_id = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .and_then(|device| device_id(&device))
            .ok();

        let collection = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;
        let mut devices = Vec::with_capacity(count as usize);
        for i in 0..count {
            let device = collection.Item(i)?;
            let id = device_id(&device)?;
            devices.push(DeviceInfo {
                is_default: default_id.as_deref() == Some(id.as_str()),
                name: friendly_name(&device)?,
                mix_format: activate_client(&device)?.format,
                id,
            });
        }
        Ok(devices)
    }
}

/// Opens an endpoint by its IMMDevice ID string.
pub fn get_device(id: &str) -> Result<IMMDevice> {
    unsafe {
        let enumerator = create_enumerator()?;
        enumerator.GetDevice(&HSTRING::from(id))
    }
}

/// An activated IAudioClient together with the endpoint's parsed mix format
/// and the full WAVEFORMATEX blob needed for IAudioClient::Initialize.
pub struct ClientHandle {
    pub client: IAudioClient,
    pub format: MixFormat,
    format_blob: Vec<u8>,
}

impl ClientHandle {
    /// Pointer to the complete mix format (WAVEFORMATEX header plus cbSize
    /// extension bytes), valid as long as this handle lives.
    pub fn format_ptr(&self) -> *const WAVEFORMATEX {
        self.format_blob.as_ptr() as *const WAVEFORMATEX
    }
}

/// Activates an IAudioClient on the device and reads its mix format.
pub fn activate_client(device: &IMMDevice) -> Result<ClientHandle> {
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let format_ptr = client.GetMixFormat()?;
        let header = *format_ptr;
        let blob_len = std::mem::size_of::<WAVEFORMATEX>() + header.cbSize as usize;
        let format_blob = std::slice::from_raw_parts(format_ptr as *const u8, blob_len).to_vec();
        let format = parse_format(format_ptr);
        CoTaskMemFree(Some(format_ptr as *const c_void));
        Ok(ClientHandle {
            client,
            format,
            format_blob,
        })
    }
}

unsafe fn device_id(device: &IMMDevice) -> Result<String> {
    unsafe {
        let pwstr = device.GetId()?;
        let id = pwstr.to_hstring().to_string_lossy();
        CoTaskMemFree(Some(pwstr.0 as *const c_void));
        Ok(id)
    }
}

unsafe fn friendly_name(device: &IMMDevice) -> Result<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        let value = store.GetValue(&PKEY_Device_FriendlyName)?;
        Ok(value.to_string())
    }
}

unsafe fn parse_format(format_ptr: *const WAVEFORMATEX) -> MixFormat {
    unsafe {
        let format = *format_ptr;
        let sample_type = match format.wFormatTag {
            WAVE_FORMAT_TAG_PCM => SampleType::Pcm,
            WAVE_FORMAT_TAG_IEEE_FLOAT => SampleType::Float,
            WAVE_FORMAT_TAG_EXTENSIBLE => {
                let extensible = *(format_ptr as *const WAVEFORMATEXTENSIBLE);
                match extensible.SubFormat.data1 {
                    SUBTYPE_DATA1_PCM => SampleType::Pcm,
                    SUBTYPE_DATA1_IEEE_FLOAT => SampleType::Float,
                    _ => SampleType::Other(format.wFormatTag),
                }
            }
            other => SampleType::Other(other),
        };
        MixFormat {
            sample_rate: format.nSamplesPerSec,
            channels: format.nChannels,
            bits_per_sample: format.wBitsPerSample,
            sample_type,
        }
    }
}
