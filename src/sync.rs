//! Playback alignment in source frames, including the output pipeline.
//!
//! IAudioClock position/frequency measures played frames, not just data
//! handed to the audio engine. Submitted minus played therefore includes
//! endpoint queues and the latency reported by the driver. QPC extrapolation
//! puts each measurement at the time the source ring is sampled.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub struct SyncBudget {
    reserve_frames: u64,
    target_frames: AtomicU64,
    preparing: AtomicUsize,
}

impl SyncBudget {
    pub fn new(reserve_frames: u64, targets: usize) -> Arc<Self> {
        Arc::new(Self {
            reserve_frames,
            target_frames: AtomicU64::new(reserve_frames),
            preparing: AtomicUsize::new(targets),
        })
    }

    /// Keep enough source headroom for the slowest initialized output.
    /// The budget only increases during a session, avoiding timeline jumps
    /// when a slow device is removed or another device rejoins.
    pub fn include_pipeline(&self, frames: u64) {
        self.target_frames
            .fetch_max(self.reserve_frames + frames, Ordering::AcqRel);
    }

    pub fn target_frames(&self) -> u64 {
        self.target_frames.load(Ordering::Acquire)
    }

    pub fn ready(&self) -> bool {
        self.preparing.load(Ordering::Acquire) == 0
    }

    /// An error during preparation must release the startup gate as well.
    pub fn preparation(self: &Arc<Self>) -> Preparation {
        Preparation(Arc::clone(self))
    }
}

pub struct Preparation(Arc<SyncBudget>);

impl Drop for Preparation {
    fn drop(&mut self) {
        self.0.preparing.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Convert a clock snapshot to output frames at the current QPC timestamp.
/// WASAPI supplies its correlated QPC in 100 ns units, not raw QPC ticks.
pub fn played_frames(
    position: u64,
    frequency: u64,
    rate: u32,
    clock_qpc_hns: u64,
    now_qpc_hns: u64,
) -> f64 {
    let elapsed = now_qpc_hns.saturating_sub(clock_qpc_hns) as f64 / 10_000_000.0;
    (position as f64 / frequency as f64 + elapsed) * f64::from(rate)
}

/// Pending output in source-frame units, including resampler history.
/// Use the current output/input ratio when interpreting queued output.
pub fn pending_source_frames(
    submitted: f64,
    played: f64,
    resampler_delay: usize,
    ratio: f64,
) -> f64 {
    ((submitted - played).max(0.0) + resampler_delay as f64) / ratio
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_units_and_qpc_are_converted_to_frames() {
        // A byte-based stereo f32 clock: 384 kB/s, 96000 bytes at 250 ms.
        assert_eq!(played_frames(96000, 384000, 48000, 100, 100100), 12480.0);
    }

    #[test]
    fn different_output_queues_have_equal_total_playback_lag() {
        let budget = SyncBudget::new(4800, 2);
        budget.include_pipeline(480);
        budget.include_pipeline(1920);
        let target = budget.target_frames() as f64;
        let fast_queue = pending_source_frames(2000.0, 1520.0, 0, 1.0);
        let slow_queue = pending_source_frames(6000.0, 4080.0, 0, 1.0);
        let fast_ring = target - fast_queue;
        let slow_ring = target - slow_queue;
        assert_eq!(fast_ring - slow_ring, 1440.0);
        assert_eq!(fast_ring + fast_queue, slow_ring + slow_queue);
        // Equal ring fill would leave a 30 ms playback offset.
        assert_ne!(4800.0 + fast_queue, 4800.0 + slow_queue);
    }

    #[test]
    fn resampler_delay_and_rate_conversion_contribute_to_lag() {
        assert_eq!(pending_source_frames(1000.0, 600.0, 41, 0.5), 882.0);
    }

    #[test]
    fn preparation_errors_release_gate_and_removal_keeps_budget() {
        let budget = SyncBudget::new(4800, 2);
        let a = budget.preparation();
        let b = budget.preparation();
        budget.include_pipeline(1920);
        drop(a);
        assert!(!budget.ready());
        drop(b);
        assert!(budget.ready());
        budget.include_pipeline(480);
        assert_eq!(budget.target_frames(), 6720);
    }
}
