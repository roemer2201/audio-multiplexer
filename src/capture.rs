//! WASAPI loopback capture of a render endpoint.
//!
//! The captured stream is converted to the canonical format (interleaved
//! stereo f32 at the source endpoint's mix rate) before it is handed to the
//! sink.
//!
//! Loopback specifics (see
//! https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording):
//! - A loopback capture client does not reliably signal event-driven buffer
//!   availability on its own (the documented workaround is a parallel render
//!   client), so this implementation polls with a short sleep instead. The
//!   200 ms capture buffer makes the 5 ms poll interval uncritical.
//! - The audio engine only produces loopback packets while at least one
//!   stream is playing on the endpoint; during silence no packets arrive and
//!   the canonical timeline simply pauses. Downstream consumers must handle
//!   such gaps (the render side rebuffers).

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient,
};

use crate::com::ComGuard;
use crate::devices::{self, MixFormat, StreamSampleKind};

/// Sleep between capture polls; small compared to the capture buffer.
pub const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Capture buffer duration in 100 ns units (200 ms).
const BUFFER_DURATION_HNS: i64 = 2_000_000;

pub struct LoopbackCapture {
    handle: devices::ClientHandle,
    capture: IAudioCaptureClient,
    kind: StreamSampleKind,
    scratch: Vec<f32>,
    pub frames_captured: u64,
    pub discontinuities: u64,
}

impl LoopbackCapture {
    /// Opens a loopback capture on the endpoint. COM must be initialized on
    /// the calling thread, which must also keep driving `drain`.
    pub fn open(device_id: &str) -> Result<Self> {
        let device = devices::get_device(device_id).context("opening source device")?;
        let handle = devices::activate_client(&device).context("activating source device")?;
        let kind = handle.format.stream_kind().with_context(|| {
            format!(
                "unsupported source mix format ({}); only 32-bit float and 16-bit PCM are supported",
                handle.format
            )
        })?;
        unsafe {
            handle.client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER_DURATION_HNS,
                0,
                handle.format_ptr(),
                None,
            )?;
        }
        let capture: IAudioCaptureClient = unsafe { handle.client.GetService()? };
        Ok(Self {
            handle,
            capture,
            kind,
            scratch: Vec::new(),
            frames_captured: 0,
            discontinuities: 0,
        })
    }

    pub fn format(&self) -> &MixFormat {
        &self.handle.format
    }

    pub fn start(&self) -> Result<()> {
        unsafe { self.handle.client.Start()? };
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        unsafe { self.handle.client.Stop()? };
        Ok(())
    }

    /// Drains all pending capture packets. Each packet is converted to
    /// interleaved stereo f32 and passed to `sink` in order.
    pub fn drain(&mut self, sink: &mut dyn FnMut(&[f32])) -> Result<()> {
        unsafe {
            loop {
                if self.capture.GetNextPacketSize()? == 0 {
                    return Ok(());
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                    self.discontinuities += 1;
                }
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                convert_to_stereo_f32(
                    data,
                    frames as usize,
                    self.handle.format.channels as usize,
                    self.kind,
                    silent,
                    &mut self.scratch,
                );
                self.capture.ReleaseBuffer(frames)?;
                self.frames_captured += frames as u64;
                sink(&self.scratch);
            }
        }
    }
}

/// Converts one WASAPI capture packet into interleaved stereo f32.
///
/// Channel mapping: mono is duplicated to both sides, layouts with more than
/// two channels contribute their first two channels only.
unsafe fn convert_to_stereo_f32(
    data: *const u8,
    frames: usize,
    channels: usize,
    kind: StreamSampleKind,
    silent: bool,
    out: &mut Vec<f32>,
) {
    out.clear();
    out.reserve(frames * 2);
    if silent {
        out.extend(std::iter::repeat_n(0.0f32, frames * 2));
        return;
    }
    match kind {
        StreamSampleKind::Float32 => {
            let samples =
                unsafe { std::slice::from_raw_parts(data as *const f32, frames * channels) };
            for frame in samples.chunks_exact(channels) {
                let (left, right) = spread_channels(frame);
                out.push(left);
                out.push(right);
            }
        }
        StreamSampleKind::Pcm16 => {
            let samples =
                unsafe { std::slice::from_raw_parts(data as *const i16, frames * channels) };
            for frame in samples.chunks_exact(channels) {
                let left = frame[0] as f32 / 32768.0;
                let right = frame[frame.len().min(2) - 1] as f32 / 32768.0;
                out.push(left);
                out.push(right);
            }
        }
    }
}

fn spread_channels(frame: &[f32]) -> (f32, f32) {
    match frame.len() {
        1 => (frame[0], frame[0]),
        _ => (frame[0], frame[1]),
    }
}

/// Records the loopback stream of `device_id` into a WAV file (stereo f32 at
/// the source mix rate). Runs for `seconds` of wall-clock time; the WAV only
/// grows while audio is actually playing on the endpoint (loopback delivers
/// no packets during silence).
pub fn record_to_wav(device_id: &str, seconds: u64, path: &Path) -> Result<()> {
    let _com = ComGuard::new()?;
    let mut capture = LoopbackCapture::open(device_id)?;
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: capture.format().sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("creating {}", path.display()))?;

    println!(
        "Recording {} s of loopback audio at {} into {} ...",
        seconds,
        capture.format(),
        path.display()
    );
    capture.start()?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut pending: Vec<f32> = Vec::new();
    while Instant::now() < deadline {
        std::thread::sleep(POLL_INTERVAL);
        capture.drain(&mut |chunk| pending.extend_from_slice(chunk))?;
        for &sample in &pending {
            writer.write_sample(sample)?;
        }
        pending.clear();
    }
    capture.stop()?;
    writer.finalize()?;

    ensure!(
        capture.frames_captured > 0,
        "no audio was captured; loopback only delivers data while something is playing on the source endpoint"
    );
    println!(
        "Wrote {} frames ({:.1} s of audio), {} discontinuities.",
        capture.frames_captured,
        capture.frames_captured as f64 / f64::from(capture.format().sample_rate),
        capture.discontinuities
    );
    Ok(())
}
