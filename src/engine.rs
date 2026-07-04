//! Fan-out engine: one source thread feeding N per-device render threads
//! through the broadcast ring buffer.
//!
//! Failure isolation: a failing render device only takes down its own thread
//! (marked as failed in the status output); the source and the remaining
//! devices keep running. A failing source stops the whole engine.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};

use crate::capture::{LoopbackCapture, POLL_INTERVAL};
use crate::com::ComGuard;
use crate::render::{self, RenderParams};
use crate::ring::Ring;
use crate::tone::{TONE_RATE, run_tone_source};

/// Ring capacity in seconds of canonical audio.
const RING_SECONDS: usize = 4;

/// Per-device buffering target as a fraction of the source rate (100 ms).
/// All devices aim for the same fill level, which keeps them mutually in
/// sync and leaves headroom for the later per-device delay feature.
const TARGET_FILL_DIVISOR: u64 = 10;

const STATUS_INTERVAL: Duration = Duration::from_secs(5);

pub enum Source {
    Loopback { device_id: String, sample_rate: u32 },
    Tone,
}

/// Shared per-device volume: the control side sets a target gain, the render
/// thread ramps its applied gain toward it (see `render::apply_gain`).
///
/// v1 maps percent linearly to gain (0..100 -> 0.0..1.0); a perceptual dB
/// mapping is deferred to the GUI phase.
pub struct Volume {
    gain_bits: AtomicU32,
}

impl Volume {
    pub fn new(percent: u8) -> Arc<Self> {
        let volume = Arc::new(Self {
            gain_bits: AtomicU32::new(0),
        });
        volume.set_percent(percent);
        volume
    }

    pub fn set_percent(&self, percent: u8) {
        let gain = f32::from(percent.min(100)) / 100.0;
        self.gain_bits.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed))
    }

    pub fn percent(&self) -> u8 {
        (self.gain() * 100.0).round() as u8
    }
}

pub struct Target {
    pub id: String,
    pub name: String,
    pub volume: Arc<Volume>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    Starting,
    Rebuffering,
    Running,
    Failed,
}

impl EngineState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Rebuffering,
            2 => Self::Running,
            3 => Self::Failed,
            _ => Self::Starting,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Rebuffering => "rebuffering",
            Self::Running => "running",
            Self::Failed => "failed",
        }
    }
}

/// Shared per-device counters, written by the render thread and read by the
/// status loop.
pub struct DeviceStats {
    pub name: String,
    volume: Arc<Volume>,
    state: AtomicU8,
    underruns: AtomicU64,
    overruns: AtomicU64,
    fill_ms: AtomicU64,
    drift_ppm: AtomicI64,
}

impl DeviceStats {
    fn new(name: String, volume: Arc<Volume>) -> Self {
        Self {
            name,
            volume,
            state: AtomicU8::new(EngineState::Starting as u8),
            underruns: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            fill_ms: AtomicU64::new(0),
            drift_ppm: AtomicI64::new(0),
        }
    }

    pub fn set_state(&self, state: EngineState) {
        self.state.store(state as u8, Ordering::Relaxed);
    }

    pub fn add_underrun(&self) {
        self.underruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_overrun(&self) {
        self.overruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_fill_ms(&self, fill_ms: u64) {
        self.fill_ms.store(fill_ms, Ordering::Relaxed);
    }

    pub fn set_drift_ppm(&self, ppm: i64) {
        self.drift_ppm.store(ppm, Ordering::Relaxed);
    }

    fn status_line(&self, index: usize) -> String {
        format!(
            "  [{index}] {}: state={} vol={}% fill={}ms drift={:+}ppm underruns={} overruns={}",
            self.name,
            EngineState::from_u8(self.state.load(Ordering::Relaxed)).as_str(),
            self.volume.percent(),
            self.fill_ms.load(Ordering::Relaxed),
            self.drift_ppm.load(Ordering::Relaxed),
            self.underruns.load(Ordering::Relaxed),
            self.overruns.load(Ordering::Relaxed),
        )
    }
}

/// Runs the engine until Enter is pressed, `seconds` elapse (if given), or
/// the source fails.
pub fn run(source: Source, targets: Vec<Target>, seconds: Option<u64>) -> Result<()> {
    let source_rate = match &source {
        Source::Loopback { sample_rate, .. } => *sample_rate,
        Source::Tone => TONE_RATE,
    };
    let ring = Ring::new(source_rate as usize * RING_SECONDS);
    let target_fill_frames = u64::from(source_rate) / TARGET_FILL_DIVISOR;
    let stop = Arc::new(AtomicBool::new(false));

    let mut stats_list = Vec::new();
    let mut render_handles = Vec::new();
    for target in &targets {
        let stats = Arc::new(DeviceStats::new(
            target.name.clone(),
            Arc::clone(&target.volume),
        ));
        stats_list.push(Arc::clone(&stats));
        let params = RenderParams {
            device_id: target.id.clone(),
            source_rate,
            target_fill_frames,
            volume: Arc::clone(&target.volume),
            stats: Arc::clone(&stats),
        };
        let reader = ring.reader();
        let stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name(format!("render {}", target.name))
            .spawn(move || {
                if let Err(err) = render::run(params, reader, stop) {
                    stats.set_state(EngineState::Failed);
                    eprintln!("render device '{}' failed: {err:#}", stats.name);
                }
            })
            .context("spawning render thread")?;
        render_handles.push(handle);
    }

    let source_handle = {
        let ring = Arc::clone(&ring);
        let stop = Arc::clone(&stop);
        thread::Builder::new()
            .name("source".to_string())
            .spawn(move || {
                let result = match source {
                    Source::Loopback {
                        device_id,
                        sample_rate,
                    } => run_loopback_source(&device_id, sample_rate, &ring, &stop),
                    Source::Tone => run_tone_source(&ring, &stop),
                };
                if let Err(err) = result {
                    eprintln!("source failed: {err:#}");
                }
                // Without a source the engine cannot continue.
                stop.store(true, Ordering::Relaxed);
            })
            .context("spawning source thread")?
    };

    println!("Engine running.");
    println!("Commands: 'v <target#> <0-100>' sets a device volume, Enter or 'q' stops.");
    {
        let stop = Arc::clone(&stop);
        let volumes: Vec<(String, Arc<Volume>)> = targets
            .iter()
            .map(|t| (t.name.clone(), Arc::clone(&t.volume)))
            .collect();
        // Detached on purpose: read_line cannot be interrupted, the thread
        // ends with the process.
        thread::spawn(move || command_loop(&volumes, &stop));
    }

    let started = Instant::now();
    let mut last_status = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(200));
        if let Some(limit) = seconds
            && started.elapsed() >= Duration::from_secs(limit)
        {
            stop.store(true, Ordering::Relaxed);
        }
        if last_status.elapsed() >= STATUS_INTERVAL {
            last_status = Instant::now();
            println!("status after {} s:", started.elapsed().as_secs());
            print_status(&stats_list);
        }
    }
    stop.store(true, Ordering::Relaxed);

    for handle in render_handles {
        let _ = handle.join();
    }
    let _ = source_handle.join();

    println!("final status:");
    print_status(&stats_list);
    Ok(())
}

fn print_status(stats_list: &[Arc<DeviceStats>]) {
    for (index, stats) in stats_list.iter().enumerate() {
        println!("{}", stats.status_line(index));
    }
}

/// Minimal runtime control channel over stdin; the same command set will be
/// driven by the GUI through a proper channel interface in a later phase.
fn command_loop(volumes: &[(String, Arc<Volume>)], stop: &AtomicBool) {
    let stdin = std::io::stdin();
    let mut line = String::new();
    while !stop.load(Ordering::Relaxed) {
        line.clear();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            // EOF: no interactive control available; keep the engine running.
            return;
        }
        let command = line.trim();
        if command.is_empty() || command.eq_ignore_ascii_case("q") {
            stop.store(true, Ordering::Relaxed);
            return;
        }
        match parse_volume_command(command, volumes.len()) {
            Ok((index, percent)) => {
                let (name, volume) = &volumes[index];
                volume.set_percent(percent);
                println!("volume of [{index}] {name} set to {percent}%");
            }
            Err(reason) => println!("{reason} (usage: 'v <target#> <0-100>', Enter or 'q' stops)"),
        }
    }
}

fn parse_volume_command(command: &str, target_count: usize) -> Result<(usize, u8), String> {
    let mut parts = command.split_ascii_whitespace();
    if parts.next() != Some("v") {
        return Err(format!("unknown command '{command}'"));
    }
    let index = parts
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|i| *i < target_count)
        .ok_or_else(|| format!("expected a target index between 0 and {}", target_count - 1))?;
    let percent = parts
        .next()
        .and_then(|s| s.parse::<u8>().ok())
        .filter(|p| *p <= 100)
        .ok_or_else(|| "expected a volume between 0 and 100".to_string())?;
    if parts.next().is_some() {
        return Err("too many arguments".to_string());
    }
    Ok((index, percent))
}

fn run_loopback_source(
    device_id: &str,
    expected_rate: u32,
    ring: &Arc<Ring>,
    stop: &AtomicBool,
) -> Result<()> {
    let _com = ComGuard::new()?;
    let mut capture = LoopbackCapture::open(device_id)?;
    ensure!(
        capture.format().sample_rate == expected_rate,
        "source sample rate changed between setup and start ({} vs {})",
        capture.format().sample_rate,
        expected_rate
    );
    capture.start()?;
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(POLL_INTERVAL);
        capture.drain(&mut |chunk| ring.write(chunk))?;
    }
    capture.stop()?;
    if capture.discontinuities > 0 {
        println!(
            "note: {} capture discontinuities occurred",
            capture.discontinuities
        );
    }
    Ok(())
}
