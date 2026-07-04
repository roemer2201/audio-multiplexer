//! Event-driven WASAPI render thread with per-device adaptive resampling.
//!
//! Each render thread pulls the canonical stream (interleaved stereo f32 at
//! the source rate) from its ring buffer reader, resamples it to the device
//! mix rate, converts sample type and channel count, and writes into the
//! device's shared-mode buffer.
//!
//! Clock-drift compensation: the device consumes at its own clock, the source
//! produces at the source clock. Any rate mismatch shows up as a trend in the
//! ring buffer fill level, so a slow PI controller steers the resampling
//! ratio to hold the fill level at a fixed target. This compensates both
//! nominal rate differences and slow clock drift without relying on device
//! timestamps. The approach and its tuning are documented in docs/drift.md.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, PolynomialDegree, Resampler};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    IAudioRenderClient,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::com::ComGuard;
use crate::devices::{self, StreamSampleKind};
use crate::engine::{DeviceStats, EngineState, Volume};
use crate::ring::{CHANNELS, ReadError, Reader};

/// Allowed adjustment range of the resampling ratio at construction time.
const MAX_RATIO_RELATIVE: f64 = 1.25;

/// How often the drift controller updates the resampling ratio.
const CONTROL_INTERVAL: Duration = Duration::from_millis(100);

/// Proportional gain on the normalized fill-level error.
const GAIN_P: f64 = 0.05;

/// Integral gain per control interval on the normalized fill-level error.
const GAIN_I: f64 = 0.005;

/// Anti-windup clamp for the integral term (absolute ratio correction).
const INTEGRAL_LIMIT: f64 = 0.005;

/// Hard clamp for the total ratio correction (2 percent).
const CORRECTION_LIMIT: f64 = 0.02;

/// Smoothing factor for the fill-level EMA, applied once per render pass.
const FILL_EMA_ALPHA: f64 = 0.1;

/// Volume ramp time from gain 0.0 to 1.0. Ramping the gain instead of
/// switching it instantly avoids zipper noise and clicks.
const GAIN_RAMP_SECONDS: f32 = 0.010;

pub struct RenderParams {
    pub device_id: String,
    pub source_rate: u32,
    pub target_fill_frames: u64,
    pub volume: Arc<Volume>,
    pub stats: Arc<DeviceStats>,
}

/// Closes the WASAPI wakeup event handle on drop.
struct EventHandle(HANDLE);

impl Drop for EventHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn run(params: RenderParams, mut reader: Reader, stop: Arc<AtomicBool>) -> Result<()> {
    let stats = &params.stats;
    let _com = ComGuard::new()?;
    let device = devices::get_device(&params.device_id).context("opening target device")?;
    let handle = devices::activate_client(&device).context("activating target device")?;
    let kind = handle.format.stream_kind().with_context(|| {
        format!(
            "unsupported device mix format ({}); only 32-bit float and 16-bit PCM are supported",
            handle.format
        )
    })?;
    let device_channels = handle.format.channels as usize;
    let device_rate = handle.format.sample_rate;

    unsafe {
        // Buffer duration 0 lets the engine pick the default period for
        // event-driven shared mode.
        handle.client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0,
            0,
            handle.format_ptr(),
            None,
        )?;
    }
    let buffer_frames = unsafe { handle.client.GetBufferSize()? };
    let event = EventHandle(unsafe { CreateEventW(None, false, false, None)? });
    unsafe { handle.client.SetEventHandle(event.0)? };
    let render: IAudioRenderClient = unsafe { handle.client.GetService()? };

    let base_ratio = f64::from(device_rate) / f64::from(params.source_rate);
    let mut resampler = Async::<f32>::new_poly(
        base_ratio,
        MAX_RATIO_RELATIVE,
        PolynomialDegree::Septic,
        buffer_frames as usize,
        CHANNELS,
        FixedAsync::Output,
    )?;
    let mut in_buf = vec![0.0f32; resampler.input_frames_max() * CHANNELS];
    let mut out_buf = vec![0.0f32; buffer_frames as usize * CHANNELS];

    // Pre-fill the device buffer with silence so the stream starts cleanly.
    unsafe {
        let _ = render.GetBuffer(buffer_frames)?;
        render.ReleaseBuffer(buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
    }
    unsafe { handle.client.Start()? };

    let mut controller = DriftController::new(params.target_fill_frames as f64);
    let mut rebuffering = true;
    stats.set_state(EngineState::Rebuffering);

    // Per-frame gain step so a full 0.0 -> 1.0 change takes GAIN_RAMP_SECONDS.
    let gain_step = 1.0 / (GAIN_RAMP_SECONDS * device_rate as f32);
    let mut current_gain = params.volume.gain();

    while !stop.load(Ordering::Relaxed) {
        let wait = unsafe { WaitForSingleObject(event.0, 2000) };
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if wait == WAIT_FAILED {
            return Err(windows::core::Error::from_thread()).context("waiting for render event");
        }
        if wait != WAIT_OBJECT_0 {
            // Timeout; loop to re-check the stop flag.
            continue;
        }
        let padding = unsafe { handle.client.GetCurrentPadding()? };
        let frames_out = buffer_frames - padding;
        if frames_out == 0 {
            continue;
        }

        // After start or an underrun, wait until the source has produced a
        // full target fill, then latch onto the stream exactly target frames
        // behind the writer.
        if rebuffering {
            if reader.available() >= params.target_fill_frames {
                reader.seek_to_latest(params.target_fill_frames);
                controller.reset();
                rebuffering = false;
                stats.set_state(EngineState::Running);
            } else {
                write_silence(&render, frames_out)?;
                continue;
            }
        }

        resampler
            .set_chunk_size(frames_out as usize)
            .map_err(|e| anyhow!("resampler chunk size: {e}"))?;
        let frames_in = resampler.input_frames_next();
        match reader.read_exact(&mut in_buf[..frames_in * CHANNELS]) {
            Ok(()) => {}
            Err(ReadError::NotEnough) => {
                stats.add_underrun();
                stats.set_state(EngineState::Rebuffering);
                rebuffering = true;
                write_silence(&render, frames_out)?;
                continue;
            }
            Err(ReadError::Overwritten) => {
                stats.add_overrun();
                reader.seek_to_latest(params.target_fill_frames);
                controller.reset();
                write_silence(&render, frames_out)?;
                continue;
            }
        }

        let input = InterleavedSlice::new(&in_buf[..frames_in * CHANNELS], CHANNELS, frames_in)
            .map_err(|e| anyhow!("input adapter: {e:?}"))?;
        let mut output = InterleavedSlice::new_mut(&mut out_buf, CHANNELS, frames_out as usize)
            .map_err(|e| anyhow!("output adapter: {e:?}"))?;
        let (_, produced) = resampler
            .process_into_buffer(&input, &mut output, None)
            .map_err(|e| anyhow!("resampler: {e}"))?;

        apply_gain(
            &mut out_buf[..produced * CHANNELS],
            &mut current_gain,
            params.volume.gain(),
            gain_step,
        );

        unsafe {
            let dst = render.GetBuffer(frames_out)?;
            write_device_frames(dst, produced, device_channels, kind, &out_buf);
            // A FixedAsync::Output resampler always yields the full chunk,
            // but release only what was actually produced to stay safe.
            render.ReleaseBuffer(produced as u32, 0)?;
        }

        // Drift compensation: steer the fill level back to the target.
        let fill = reader.available();
        if let Some(correction) = controller.update(fill as f64) {
            resampler
                .set_resample_ratio_relative(1.0 - correction, true)
                .map_err(|e| anyhow!("resampler ratio: {e}"))?;
            stats.set_drift_ppm((correction * 1e6) as i64);
        }
        stats.set_fill_ms(fill * 1000 / u64::from(params.source_rate));
    }

    unsafe { handle.client.Stop()? };
    Ok(())
}

/// Applies the volume gain to interleaved canonical frames, ramping the
/// applied gain toward `target` by at most `step` per frame to avoid zipper
/// noise on volume changes.
fn apply_gain(samples: &mut [f32], current: &mut f32, target: f32, step: f32) {
    if *current == target && target == 1.0 {
        return;
    }
    for frame in samples.chunks_exact_mut(CHANNELS) {
        if *current != target {
            *current += (target - *current).clamp(-step, step);
        }
        for sample in frame {
            *sample *= *current;
        }
    }
}

fn write_silence(render: &IAudioRenderClient, frames: u32) -> Result<()> {
    unsafe {
        let _ = render.GetBuffer(frames)?;
        render.ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
    }
    Ok(())
}

/// Writes `frames` canonical stereo frames into the device buffer, expanding
/// to the device channel count (extra channels get silence, mono gets the
/// average of left and right) and converting the sample type.
unsafe fn write_device_frames(
    dst: *mut u8,
    frames: usize,
    device_channels: usize,
    kind: StreamSampleKind,
    src: &[f32],
) {
    match kind {
        StreamSampleKind::Float32 => {
            let out = unsafe {
                std::slice::from_raw_parts_mut(dst as *mut f32, frames * device_channels)
            };
            for (i, frame) in out.chunks_exact_mut(device_channels).enumerate() {
                let (left, right) = (src[2 * i], src[2 * i + 1]);
                fill_frame(frame, left, right, |v| v);
            }
        }
        StreamSampleKind::Pcm16 => {
            let out = unsafe {
                std::slice::from_raw_parts_mut(dst as *mut i16, frames * device_channels)
            };
            for (i, frame) in out.chunks_exact_mut(device_channels).enumerate() {
                let (left, right) = (src[2 * i], src[2 * i + 1]);
                fill_frame(frame, left, right, |v| {
                    (v.clamp(-1.0, 1.0) * 32767.0) as i16
                });
            }
        }
    }
}

fn fill_frame<T: Copy + Default>(
    frame: &mut [T],
    left: f32,
    right: f32,
    convert: impl Fn(f32) -> T,
) {
    if frame.len() == 1 {
        frame[0] = convert((left + right) * 0.5);
        return;
    }
    frame[0] = convert(left);
    frame[1] = convert(right);
    for slot in frame.iter_mut().skip(2) {
        *slot = T::default();
    }
}

/// Slow PI controller on the normalized fill-level error. Positive output
/// means the input side must be consumed faster (fill above target), which
/// maps to lowering the output/input resampling ratio.
struct DriftController {
    target: f64,
    fill_ema: f64,
    integral: f64,
    last_update: Instant,
}

impl DriftController {
    fn new(target: f64) -> Self {
        Self {
            target,
            fill_ema: target,
            integral: 0.0,
            last_update: Instant::now(),
        }
    }

    fn reset(&mut self) {
        self.fill_ema = self.target;
        self.integral = 0.0;
        self.last_update = Instant::now();
    }

    /// Feeds one fill-level sample; returns a new total ratio correction
    /// once per control interval.
    fn update(&mut self, fill: f64) -> Option<f64> {
        self.fill_ema += FILL_EMA_ALPHA * (fill - self.fill_ema);
        if self.last_update.elapsed() < CONTROL_INTERVAL {
            return None;
        }
        self.last_update = Instant::now();
        let error = (self.fill_ema - self.target) / self.target;
        self.integral = (self.integral + GAIN_I * error).clamp(-INTEGRAL_LIMIT, INTEGRAL_LIMIT);
        let correction =
            (GAIN_P * error + self.integral).clamp(-CORRECTION_LIMIT, CORRECTION_LIMIT);
        Some(correction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_full_gain_leaves_samples_untouched() {
        let mut samples = vec![0.25f32; 8];
        let mut current = 1.0f32;
        apply_gain(&mut samples, &mut current, 1.0, 0.01);
        assert_eq!(samples, vec![0.25f32; 8]);
    }

    #[test]
    fn gain_ramps_toward_target_without_overshoot() {
        let mut samples = vec![1.0f32; 20];
        let mut current = 0.0f32;
        apply_gain(&mut samples, &mut current, 1.0, 0.25);
        // Each frame steps by at most 0.25 and never exceeds the target.
        let per_frame: Vec<f32> = samples.chunks_exact(CHANNELS).map(|f| f[0]).collect();
        assert_eq!(per_frame[0], 0.25);
        assert_eq!(per_frame[1], 0.5);
        assert!(per_frame.windows(2).all(|w| w[1] >= w[0]));
        assert!(per_frame.iter().all(|&g| g <= 1.0));
        assert_eq!(current, 1.0);
    }

    #[test]
    fn both_channels_of_a_frame_get_the_same_gain() {
        let mut samples = vec![1.0f32; 4];
        let mut current = 0.0f32;
        apply_gain(&mut samples, &mut current, 1.0, 0.5);
        assert_eq!(samples, vec![0.5, 0.5, 1.0, 1.0]);
    }

    #[test]
    fn zero_volume_silences_after_the_ramp() {
        let mut samples = vec![1.0f32; 100];
        let mut current = 1.0f32;
        apply_gain(&mut samples, &mut current, 0.0, 0.1);
        assert_eq!(current, 0.0);
        assert_eq!(samples[samples.len() - 1], 0.0);
    }
}
