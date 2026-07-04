//! Enumeration of audio render endpoints via IMMDeviceEnumerator.
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
    WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::core::Result;

// Format tag values from mmreg.h. Defined locally to avoid pulling in the
// whole multimedia feature set for three constants.
const WAVE_FORMAT_TAG_PCM: u16 = 0x0001;
const WAVE_FORMAT_TAG_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_TAG_EXTENSIBLE: u16 = 0xFFFE;

// Data1 fields of KSDATAFORMAT_SUBTYPE_PCM / _IEEE_FLOAT. The remaining GUID
// fields are the fixed media-type suffix 0000-0010-8000-00aa00389b71.
const SUBTYPE_DATA1_PCM: u32 = 0x0000_0001;
const SUBTYPE_DATA1_IEEE_FLOAT: u32 = 0x0000_0003;

pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub mix_format: MixFormat,
}

pub struct MixFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub sample_type: SampleType,
}

pub enum SampleType {
    Pcm,
    Float,
    Other(u16),
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

/// Lists all active render endpoints, including their shared-mode mix format.
///
/// COM must already be initialized on the calling thread (see `com::ComGuard`).
pub fn list_render_devices() -> Result<Vec<DeviceInfo>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;

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
                mix_format: mix_format(&device)?,
                id,
            });
        }
        Ok(devices)
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

unsafe fn mix_format(device: &IMMDevice) -> Result<MixFormat> {
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let format_ptr = client.GetMixFormat()?;
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

        let mix_format = MixFormat {
            sample_rate: format.nSamplesPerSec,
            channels: format.nChannels,
            bits_per_sample: format.wBitsPerSample,
            sample_type,
        };
        CoTaskMemFree(Some(format_ptr as *const c_void));
        Ok(mix_format)
    }
}
