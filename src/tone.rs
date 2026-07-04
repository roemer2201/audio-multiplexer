//! Synthetic test-tone source for sync measurements (Phase 5 tooling).
//!
//! Generates a short 1 kHz burst once per second into the canonical stream.
//! Recording the outputs of two devices and comparing the burst positions
//! over time reveals the residual drift between them; see docs/drift.md.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::ring::Ring;

/// Canonical rate of the generated stream.
pub const TONE_RATE: u32 = 48000;

/// Frames per generated chunk (10 ms at 48 kHz).
const CHUNK_FRAMES: usize = 480;

/// Burst length in frames (10 ms).
const BURST_FRAMES: u64 = 480;

/// Burst repetition period in frames (1 s).
const PERIOD_FRAMES: u64 = TONE_RATE as u64;

const BURST_FREQUENCY: f64 = 1000.0;
const AMPLITUDE: f32 = 0.5;

/// Linear fade-in/out length inside the burst (1 ms), avoids hard clicks
/// that could stress tweeters while staying sharply localizable in time.
const FADE_FRAMES: u64 = 48;

/// Produces the click pattern into the ring, paced by the wall clock, until
/// `stop` is set.
pub fn run_tone_source(ring: &Arc<Ring>, stop: &AtomicBool) -> Result<()> {
    let mut buf = vec![0.0f32; CHUNK_FRAMES * 2];
    let mut total_frames: u64 = 0;
    let chunk_duration = Duration::from_millis(10);
    let mut next_deadline = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        for i in 0..CHUNK_FRAMES {
            let sample = tone_sample(total_frames + i as u64);
            buf[2 * i] = sample;
            buf[2 * i + 1] = sample;
        }
        ring.write(&buf);
        total_frames += CHUNK_FRAMES as u64;

        next_deadline += chunk_duration;
        let now = Instant::now();
        if next_deadline > now {
            std::thread::sleep(next_deadline - now);
        } else {
            // Fell behind (e.g. after a scheduler stall); resynchronize.
            next_deadline = now;
        }
    }
    Ok(())
}

fn tone_sample(frame: u64) -> f32 {
    let position = frame % PERIOD_FRAMES;
    if position >= BURST_FRAMES {
        return 0.0;
    }
    let envelope = if position < FADE_FRAMES {
        position as f32 / FADE_FRAMES as f32
    } else if position >= BURST_FRAMES - FADE_FRAMES {
        (BURST_FRAMES - position) as f32 / FADE_FRAMES as f32
    } else {
        1.0
    };
    let phase =
        2.0 * std::f64::consts::PI * BURST_FREQUENCY * position as f64 / f64::from(TONE_RATE);
    AMPLITUDE * envelope * phase.sin() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_is_silent_outside_and_bounded_inside() {
        for frame in 0..2 * PERIOD_FRAMES {
            let sample = tone_sample(frame);
            let position = frame % PERIOD_FRAMES;
            if position >= BURST_FRAMES {
                assert_eq!(sample, 0.0, "expected silence at frame {frame}");
            } else {
                assert!(sample.abs() <= AMPLITUDE, "overshoot at frame {frame}");
            }
        }
    }

    #[test]
    fn burst_fades_in_from_zero() {
        assert_eq!(tone_sample(0), 0.0);
        assert!(tone_sample(FADE_FRAMES + 12).abs() > 0.0);
    }
}
