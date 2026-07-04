//! Lock-free single-producer, multi-consumer broadcast ring buffer for
//! interleaved stereo f32 audio frames.
//!
//! One capture (or generator) thread writes the canonical stream, and every
//! render thread owns a `Reader` with an independent read position. Samples
//! are stored as `AtomicU32` (f32 bit patterns) so no locks are needed on the
//! audio path.
//!
//! Consistency model (seqlock style): the writer stores samples with relaxed
//! ordering and then publishes the new write position with a release store.
//! Readers load the write position with acquire ordering, which guarantees
//! they observe all samples up to that position. A reader that falls so far
//! behind that the writer may have reused its region gets `Overwritten` and
//! must reseek; the check is repeated after copying to reject torn data.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// The canonical stream is always interleaved stereo.
pub const CHANNELS: usize = 2;

/// Maximum frames published per chunk. Writes larger than this are split so
/// that readers can bound how far an in-flight (unpublished) write may extend
/// past the published write position.
pub const MAX_WRITE_FRAMES: usize = 4800;

pub struct Ring {
    samples: Box<[AtomicU32]>,
    capacity_frames: u64,
    write_pos: AtomicU64,
}

impl Ring {
    pub fn new(capacity_frames: usize) -> Arc<Self> {
        assert!(
            capacity_frames >= 4 * MAX_WRITE_FRAMES,
            "ring capacity too small for the write chunk safety margin"
        );
        let samples = (0..capacity_frames * CHANNELS)
            .map(|_| AtomicU32::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Arc::new(Self {
            samples,
            capacity_frames: capacity_frames as u64,
            write_pos: AtomicU64::new(0),
        })
    }

    /// Total frames ever written (monotonic).
    pub fn write_pos(&self) -> u64 {
        self.write_pos.load(Ordering::Acquire)
    }

    /// Appends interleaved stereo samples. Only one thread may call this.
    pub fn write(&self, interleaved: &[f32]) {
        assert!(interleaved.len().is_multiple_of(CHANNELS));
        let mut offset = 0;
        while offset < interleaved.len() {
            let len = (interleaved.len() - offset).min(MAX_WRITE_FRAMES * CHANNELS);
            self.write_chunk(&interleaved[offset..offset + len]);
            offset += len;
        }
    }

    fn write_chunk(&self, samples: &[f32]) {
        let start = self.write_pos.load(Ordering::Relaxed);
        let cap_samples = self.samples.len() as u64;
        let base = start * CHANNELS as u64;
        for (i, &sample) in samples.iter().enumerate() {
            let index = ((base + i as u64) % cap_samples) as usize;
            self.samples[index].store(sample.to_bits(), Ordering::Relaxed);
        }
        let frames = (samples.len() / CHANNELS) as u64;
        self.write_pos.store(start + frames, Ordering::Release);
    }

    pub fn reader(self: &Arc<Self>) -> Reader {
        Reader {
            pos: self.write_pos(),
            ring: Arc::clone(self),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    /// Fewer frames available than requested; nothing was consumed.
    NotEnough,
    /// The reader fell behind and the writer may have overwritten the
    /// requested region; the reader must reseek via `seek_to_latest`.
    Overwritten,
}

pub struct Reader {
    ring: Arc<Ring>,
    pos: u64,
}

impl Reader {
    /// Frames buffered between this reader and the writer (the fill level).
    pub fn available(&self) -> u64 {
        self.ring.write_pos().saturating_sub(self.pos)
    }

    /// Moves the read position so that `keep_frames` frames remain buffered.
    pub fn seek_to_latest(&mut self, keep_frames: u64) {
        self.pos = self.ring.write_pos().saturating_sub(keep_frames);
    }

    /// Reads exactly `dst.len() / CHANNELS` frames or fails without consuming.
    pub fn read_exact(&mut self, dst: &mut [f32]) -> Result<(), ReadError> {
        assert!(dst.len().is_multiple_of(CHANNELS));
        let frames = (dst.len() / CHANNELS) as u64;
        let write = self.ring.write_pos();
        if write.saturating_sub(self.pos) < frames {
            return Err(ReadError::NotEnough);
        }
        if !self.region_valid(write) {
            return Err(ReadError::Overwritten);
        }
        let cap_samples = self.ring.samples.len() as u64;
        let base = self.pos * CHANNELS as u64;
        for (i, slot) in dst.iter_mut().enumerate() {
            let index = ((base + i as u64) % cap_samples) as usize;
            *slot = f32::from_bits(self.ring.samples[index].load(Ordering::Relaxed));
        }
        // Re-check after copying: if the writer advanced into our region
        // meanwhile, the copied data may be torn and must be discarded.
        if !self.region_valid(self.ring.write_pos()) {
            return Err(ReadError::Overwritten);
        }
        self.pos += frames;
        Ok(())
    }

    /// The region starting at `self.pos` is safe if the writer, including one
    /// possibly in-flight unpublished chunk, has not wrapped around into it.
    fn region_valid(&self, write: u64) -> bool {
        write.saturating_sub(self.pos) <= self.ring.capacity_frames - MAX_WRITE_FRAMES as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(values: &[f32]) -> Vec<f32> {
        // Duplicates each value into a stereo frame.
        values.iter().flat_map(|&v| [v, v]).collect()
    }

    #[test]
    fn write_then_read_roundtrip() {
        let ring = Ring::new(4 * MAX_WRITE_FRAMES);
        let mut reader = ring.reader();
        ring.write(&frames(&[1.0, 2.0, 3.0]));
        assert_eq!(reader.available(), 3);
        let mut dst = vec![0.0f32; 6];
        reader.read_exact(&mut dst).unwrap();
        assert_eq!(dst, frames(&[1.0, 2.0, 3.0]));
        assert_eq!(reader.available(), 0);
    }

    #[test]
    fn read_more_than_available_fails() {
        let ring = Ring::new(4 * MAX_WRITE_FRAMES);
        let mut reader = ring.reader();
        ring.write(&frames(&[1.0]));
        let mut dst = vec![0.0f32; 4];
        assert_eq!(reader.read_exact(&mut dst), Err(ReadError::NotEnough));
        // The failed read must not consume anything.
        assert_eq!(reader.available(), 1);
    }

    #[test]
    fn lapped_reader_gets_overwritten_and_recovers() {
        let capacity = 4 * MAX_WRITE_FRAMES;
        let ring = Ring::new(capacity);
        let mut reader = ring.reader();
        // Write more than the full capacity so the reader region is reused.
        let chunk = vec![0.5f32; MAX_WRITE_FRAMES * CHANNELS];
        for _ in 0..5 {
            ring.write(&chunk);
        }
        let mut dst = vec![0.0f32; 2];
        assert_eq!(reader.read_exact(&mut dst), Err(ReadError::Overwritten));
        reader.seek_to_latest(16);
        assert_eq!(reader.available(), 16);
        reader.read_exact(&mut dst).unwrap();
        assert_eq!(dst, [0.5, 0.5]);
    }

    #[test]
    fn wraparound_preserves_sample_order() {
        let capacity = 4 * MAX_WRITE_FRAMES;
        let ring = Ring::new(capacity);
        // Advance close to the wrap point, then follow with a reader.
        let filler = vec![0.0f32; (capacity - 8) * CHANNELS];
        ring.write(&filler);
        let mut reader = ring.reader();
        let data: Vec<f32> = (0..32).map(|i| i as f32).collect();
        ring.write(&frames(&data));
        let mut dst = vec![0.0f32; 32 * CHANNELS];
        reader.read_exact(&mut dst).unwrap();
        assert_eq!(dst, frames(&data));
    }
}
