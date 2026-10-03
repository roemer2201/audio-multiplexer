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
use crate::outcome::WorkerFailure;
use crate::render::{self, RenderParams};
use crate::ring::Ring;
use crate::session::TargetChanges;
use crate::sync::SyncBudget;
use crate::tone::{TONE_RATE, run_tone_source};

/// Ring capacity in seconds of canonical audio.
const RING_SECONDS: usize = 4;

/// Per-device buffering target as a fraction of the source rate (100 ms).
/// Added to a common playback budget that includes the output pipelines.
/// Equal ring fill alone cannot align devices with different latencies.
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

    pub fn as_str(self) -> &'static str {
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
    failure: WorkerFailure,
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
            failure: WorkerFailure::default(),
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

    pub fn state(&self) -> EngineState {
        EngineState::from_u8(self.state.load(Ordering::Relaxed))
    }

    pub fn fill_ms(&self) -> u64 {
        self.fill_ms.load(Ordering::Relaxed)
    }

    pub fn drift_ppm(&self) -> i64 {
        self.drift_ppm.load(Ordering::Relaxed)
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn overruns(&self) -> u64 {
        self.overruns.load(Ordering::Relaxed)
    }

    pub fn failure(&self) -> Option<String> {
        self.failure.message()
    }

    fn status_line(&self, index: usize) -> String {
        format!(
            "  [{index}] {}: state={} vol={}% fill={}ms drift={:+}ppm underruns={} overruns={}{}",
            self.name,
            EngineState::from_u8(self.state.load(Ordering::Relaxed)).as_str(),
            self.volume.percent(),
            self.fill_ms.load(Ordering::Relaxed),
            self.drift_ppm.load(Ordering::Relaxed),
            self.underruns.load(Ordering::Relaxed),
            self.overruns.load(Ordering::Relaxed),
            self.failure()
                .map(|e| format!(" error={e}"))
                .unwrap_or_default(),
        )
    }
}

/// A started engine: the control interface for CLI and GUI frontends.
///
/// Dropping the handle stops the engine and joins all threads.
pub struct EngineHandle {
    stop: Arc<AtomicBool>,
    stats: Vec<Arc<DeviceStats>>,
    threads: Vec<thread::JoinHandle<()>>,
    renderers: Vec<RenderWorker>,
    retired: Vec<thread::JoinHandle<()>>,
    ring: Arc<Ring>,
    sync: Arc<SyncBudget>,
    source_rate: u32,
    source_failure: Arc<WorkerFailure>,
}

struct RenderWorker {
    id: String,
    stop: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

impl EngineHandle {
    pub fn target_ids(&self) -> Vec<String> {
        self.renderers
            .iter()
            .map(|worker| worker.id.clone())
            .collect()
    }

    /// Reconcile only output workers. Source and healthy readers keep their
    /// stream positions and controller state across another device's rejoin.
    pub fn reconcile_targets(&mut self, targets: &[Target]) -> Result<()> {
        let desired: Vec<String> = targets.iter().map(|t| t.id.clone()).collect();
        // A coalesced unplug/replug can leave IDs unchanged but workers dead.
        let failed: Vec<String> = self
            .renderers
            .iter()
            .enumerate()
            .filter(|(i, w)| {
                w.thread.is_finished() || self.stats[*i].state() == EngineState::Failed
            })
            .map(|(_, w)| w.id.clone())
            .collect();
        for id in failed {
            self.remove_target(&id);
        }
        let changes = TargetChanges::between(&self.target_ids(), &desired);
        for id in changes.remove {
            self.remove_target(&id);
        }
        for id in changes.add {
            let target = targets
                .iter()
                .find(|t| t.id == id)
                .expect("desired target exists");
            let (worker, stats) = spawn_renderer(
                target,
                self.source_rate,
                &self.ring,
                &self.sync,
                &self.stop,
                false,
            )?;
            self.renderers.push(worker);
            self.stats.push(stats);
        }
        self.reap_finished();
        Ok(())
    }

    fn remove_target(&mut self, id: &str) {
        if let Some(index) = self.renderers.iter().position(|worker| worker.id == id) {
            let worker = self.renderers.remove(index);
            worker.stop.store(true, Ordering::Relaxed);
            self.retired.push(worker.thread);
            self.stats.remove(index);
        }
    }

    pub fn reap_finished(&mut self) {
        let mut index = 0;
        while index < self.retired.len() {
            if self.retired[index].is_finished() {
                if self.retired.swap_remove(index).join().is_err() {
                    self.source_failure
                        .record("retired worker panicked outside its boundary".into());
                }
            } else {
                index += 1;
            }
        }
    }

    pub fn is_finished(&self) -> bool {
        self.threads.iter().all(thread::JoinHandle::is_finished)
            && self.renderers.iter().all(|w| w.thread.is_finished())
            && self.retired.iter().all(thread::JoinHandle::is_finished)
    }

    /// Per-target statistics, index-aligned with current target_ids().
    pub fn stats(&self) -> &[Arc<DeviceStats>] {
        &self.stats
    }

    /// False once a stop was requested or the source thread died.
    pub fn is_running(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
    }

    pub fn failure(&self) -> Option<String> {
        self.source_failure.message()
    }

    fn result(&self) -> Result<()> {
        if let Some(error) = self.failure() {
            anyhow::bail!("source failed: {error}");
        }
        if !self.stats.is_empty() && self.stats.iter().all(|s| s.failure().is_some()) {
            anyhow::bail!(
                "all render devices failed: {}",
                self.stats
                    .iter()
                    .map(|s| format!("{}: {}", s.name, s.failure().unwrap_or_default()))
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
        Ok(())
    }

    /// Requests a stop without blocking on the worker threads.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Stops the engine and waits for all threads to finish.
    pub fn stop(mut self) -> Result<()> {
        self.shutdown();
        self.result()
    }

    fn shutdown(&mut self) {
        self.request_stop();
        for handle in self.threads.drain(..) {
            if handle.join().is_err() {
                self.source_failure
                    .record("worker panicked outside its boundary".into());
            }
        }
        for worker in self.renderers.drain(..) {
            if worker.thread.join().is_err() {
                self.source_failure
                    .record("render worker panicked outside its boundary".into());
            }
        }
        for handle in self.retired.drain(..) {
            if handle.join().is_err() {
                self.source_failure
                    .record("worker panicked outside its boundary".into());
            }
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Starts the fan-out engine (non-blocking): one source thread plus one
/// render thread per target. Volume handles stay with the caller via the
/// `Target`s; status is exposed through `EngineHandle::stats`.
pub fn start(source: Source, targets: &[Target]) -> Result<EngineHandle> {
    start_with_spawners(
        source,
        targets,
        |target, handle| {
            spawn_renderer(
                target,
                handle.source_rate,
                &handle.ring,
                &handle.sync,
                &handle.stop,
                true,
            )
        },
        spawn_source,
    )
}

/// Own every successfully spawned worker immediately. Any subsequent error
/// drops this handle, signals stop, and joins the partial engine before the
/// error reaches the caller. Factories allow deterministic spawn-failure tests.
fn start_with_spawners(
    source: Source,
    targets: &[Target],
    mut render_spawn: impl FnMut(&Target, &EngineHandle) -> Result<(RenderWorker, Arc<DeviceStats>)>,
    source_spawn: impl FnOnce(Source, &EngineHandle) -> Result<thread::JoinHandle<()>>,
) -> Result<EngineHandle> {
    let source_rate = match &source {
        Source::Loopback { sample_rate, .. } => *sample_rate,
        Source::Tone => TONE_RATE,
    };
    let mut handle = EngineHandle {
        stop: Arc::new(AtomicBool::new(false)),
        stats: Vec::new(),
        threads: Vec::new(),
        renderers: Vec::new(),
        retired: Vec::new(),
        ring: Ring::new(source_rate as usize * RING_SECONDS),
        sync: SyncBudget::new(u64::from(source_rate) / TARGET_FILL_DIVISOR, targets.len()),
        source_rate,
        source_failure: Arc::new(WorkerFailure::default()),
    };
    for target in targets {
        let (worker, stats) = render_spawn(target, &handle)?;
        handle.renderers.push(worker);
        handle.stats.push(stats);
    }
    let source_thread = source_spawn(source, &handle)?;
    handle.threads.push(source_thread);
    Ok(handle)
}

fn spawn_source(source: Source, handle: &EngineHandle) -> Result<thread::JoinHandle<()>> {
    let ring = Arc::clone(&handle.ring);
    let stop = Arc::clone(&handle.stop);
    let failure = Arc::clone(&handle.source_failure);
    thread::Builder::new()
        .name("source".to_string())
        .spawn(move || {
            failure.run_source(&stop, || match source {
                Source::Loopback {
                    device_id,
                    sample_rate,
                } => run_loopback_source(&device_id, sample_rate, &ring, &stop),
                Source::Tone => run_tone_source(&ring, &stop),
            });
        })
        .context("spawning source thread")
}

fn spawn_renderer(
    target: &Target,
    source_rate: u32,
    ring: &Arc<Ring>,
    sync: &Arc<SyncBudget>,
    engine_stop: &Arc<AtomicBool>,
    initial_target: bool,
) -> Result<(RenderWorker, Arc<DeviceStats>)> {
    let stats = Arc::new(DeviceStats::new(
        target.name.clone(),
        Arc::clone(&target.volume),
    ));
    let params = RenderParams {
        device_id: target.id.clone(),
        source_rate,
        sync: Arc::clone(sync),
        initial_target,
        engine_stop: Arc::clone(engine_stop),
        volume: Arc::clone(&target.volume),
        stats: Arc::clone(&stats),
    };
    let reader = ring.reader();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker_stats = Arc::clone(&stats);
    let thread = thread::Builder::new()
        .name(format!("render {}", target.name))
        .spawn(move || {
            worker_stats
                .failure
                .run(|| render::run(params, reader, worker_stop));
            if let Some(error) = worker_stats.failure() {
                worker_stats.set_state(EngineState::Failed);
                eprintln!("render device '{}' failed: {error}", worker_stats.name);
            }
        })
        .context("spawning render thread")?;
    Ok((
        RenderWorker {
            id: target.id.clone(),
            stop,
            thread,
        },
        stats,
    ))
}

/// Blocking CLI frontend: runs the engine until Enter is pressed, `seconds`
/// elapse (if given), or the source fails; prints periodic status lines.
pub fn run(source: Source, targets: Vec<Target>, seconds: Option<u64>) -> Result<()> {
    let volumes: Vec<(String, Arc<Volume>)> = targets
        .iter()
        .map(|t| (t.name.clone(), Arc::clone(&t.volume)))
        .collect();
    let handle = start(source, &targets)?;

    println!("Engine running.");
    println!("Commands: 'v <target#> <0-100>' sets a device volume, Enter or 'q' stops.");
    {
        let stop = handle.stop_flag();
        // Detached on purpose: read_line cannot be interrupted, the thread
        // ends with the process.
        thread::spawn(move || command_loop(&volumes, &stop));
    }

    let started = Instant::now();
    let mut last_status = Instant::now();
    while handle.is_running() {
        thread::sleep(Duration::from_millis(200));
        if handle.result().is_err() {
            handle.request_stop();
        }
        if let Some(limit) = seconds
            && started.elapsed() >= Duration::from_secs(limit)
        {
            handle.request_stop();
        }
        if last_status.elapsed() >= STATUS_INTERVAL {
            last_status = Instant::now();
            println!("status after {} s:", started.elapsed().as_secs());
            print_status(handle.stats());
        }
    }

    let stats_list: Vec<Arc<DeviceStats>> = handle.stats().to_vec();
    let result = handle.stop();

    println!("final status:");
    print_status(&stats_list);
    result
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
    let mut capture = LoopbackCapture::open(device_id).context("opening capture")?;
    ensure!(
        capture.format().sample_rate == expected_rate,
        "source sample rate changed between setup and start ({} vs {})",
        capture.format().sample_rate,
        expected_rate
    );
    capture.start().context("starting capture")?;
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(POLL_INTERVAL);
        capture
            .drain(&mut |chunk| ring.write(chunk))
            .context("draining capture")?;
    }
    capture.stop().context("stopping capture")?;
    if capture.discontinuities > 0 {
        println!(
            "note: {} capture discontinuities occurred",
            capture.discontinuities
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Acknowledge that each fake worker really ran before returning it to
    /// the engine. Completion counters prove rollback joined those workers.
    fn synthetic_thread(
        stop: &Arc<AtomicBool>,
        completed: &Arc<AtomicUsize>,
    ) -> thread::JoinHandle<()> {
        let stop = Arc::clone(stop);
        let completed = Arc::clone(completed);
        let (ready, started) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            ready.send(()).unwrap();
            while !stop.load(Ordering::Relaxed) {
                thread::yield_now();
            }
            completed.fetch_add(1, Ordering::SeqCst);
        });
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        worker
    }

    fn synthetic_renderer(
        target: &Target,
        handle: &EngineHandle,
        completed: &Arc<AtomicUsize>,
    ) -> (RenderWorker, Arc<DeviceStats>) {
        (
            RenderWorker {
                id: target.id.clone(),
                stop: Arc::new(AtomicBool::new(false)),
                thread: synthetic_thread(&handle.stop, completed),
            },
            Arc::new(DeviceStats::new(
                target.name.clone(),
                Arc::clone(&target.volume),
            )),
        )
    }

    #[test]
    fn partial_start_joins_renderers_before_returning_spawn_error() {
        let targets: Vec<Target> = ["a", "b"]
            .into_iter()
            .map(|id| Target {
                id: id.into(),
                name: id.into(),
                volume: Volume::new(100),
            })
            .collect();
        for fail_render in [true, false] {
            let completed = Arc::new(AtomicUsize::new(0));
            let result = start_with_spawners(
                Source::Tone,
                &targets,
                |target, handle| {
                    if fail_render && target.id == "b" {
                        anyhow::bail!("injected render spawn failure");
                    }
                    Ok(synthetic_renderer(target, handle, &completed))
                },
                |_, _| anyhow::bail!("injected source spawn failure"),
            );
            let error = result.err().expect("start must fail").to_string();
            assert_eq!(
                error,
                if fail_render {
                    "injected render spawn failure"
                } else {
                    "injected source spawn failure"
                }
            );
            assert_eq!(
                completed.load(Ordering::SeqCst),
                if fail_render { 1 } else { 2 }
            );
        }
    }

    #[test]
    fn complete_start_keeps_workers_until_explicit_stop() {
        let completed = Arc::new(AtomicUsize::new(0));
        let target = Target {
            id: "a".into(),
            name: "a".into(),
            volume: Volume::new(100),
        };
        let handle = start_with_spawners(
            Source::Tone,
            &[target],
            |target, handle| Ok(synthetic_renderer(target, handle, &completed)),
            |_, handle| Ok(synthetic_thread(&handle.stop, &completed)),
        )
        .unwrap();
        assert!(handle.is_running());
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        handle.stop().unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 2);
    }

    fn idle_handle() -> EngineHandle {
        EngineHandle {
            stop: Arc::new(AtomicBool::new(false)),
            stats: Vec::new(),
            threads: Vec::new(),
            renderers: Vec::new(),
            retired: Vec::new(),
            ring: Ring::new(TONE_RATE as usize * RING_SECONDS),
            sync: SyncBudget::new(4800, 0),
            source_rate: TONE_RATE,
            source_failure: Arc::new(WorkerFailure::default()),
        }
    }

    #[test]
    fn source_failure_reaches_the_frontend_shutdown_result() {
        for stage in ["opening capture", "starting capture", "draining capture"] {
            let mut handle = idle_handle();
            let failure = Arc::clone(&handle.source_failure);
            let stop = Arc::clone(&handle.stop);
            handle.threads.push(thread::spawn(move || {
                failure.run_source(&stop, || anyhow::bail!("{stage}: injected failure"));
            }));
            let error = handle.stop().unwrap_err().to_string();
            assert_eq!(error, format!("source failed: {stage}: injected failure"));
        }
        idle_handle().stop().unwrap();
    }

    #[test]
    fn one_failed_output_is_isolated_but_all_failed_outputs_are_an_error() {
        let mut handle = idle_handle();
        let failed = Arc::new(DeviceStats::new("failed".into(), Volume::new(100)));
        failed.failure.record("device removed".into());
        failed.set_state(EngineState::Failed);
        handle.stats.push(failed);
        assert!(handle.result().is_err());
        handle.stats.push(Arc::new(DeviceStats::new(
            "healthy".into(),
            Volume::new(100),
        )));
        assert!(handle.is_running());
        handle.stop().unwrap();
    }

    #[test]
    fn unexpected_join_panic_is_not_silently_discarded() {
        let mut handle = idle_handle();
        handle
            .threads
            .push(thread::spawn(|| panic!("outside boundary")));
        assert!(handle.stop().unwrap_err().to_string().contains("panicked"));
    }

    /// Exercise actual worker reconciliation with synthetic workers, so no
    /// audio device is needed even when this test runs in Windows CI.
    #[test]
    fn removing_one_worker_does_not_stop_or_replace_the_other() {
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let volume = Volume::new(100);
        let make_worker = |id: &str, progress: Arc<AtomicU64>| {
            let local_stop = Arc::new(AtomicBool::new(false));
            let worker_stop = Arc::clone(&local_stop);
            let engine_stop = Arc::clone(&stop);
            RenderWorker {
                id: id.into(),
                stop: local_stop,
                thread: thread::spawn(move || {
                    while !worker_stop.load(Ordering::Relaxed)
                        && !engine_stop.load(Ordering::Relaxed)
                    {
                        progress.fetch_add(1, Ordering::Relaxed);
                        thread::yield_now();
                    }
                }),
            }
        };
        let a = make_worker("a", Arc::clone(&progress));
        let a_thread = a.thread.thread().id();
        let b = make_worker("b", Arc::new(AtomicU64::new(0)));
        let b_stop = Arc::clone(&b.stop);
        let mut handle = EngineHandle {
            stop: Arc::clone(&stop),
            stats: vec![
                Arc::new(DeviceStats::new("a".into(), Arc::clone(&volume))),
                Arc::new(DeviceStats::new("b".into(), Arc::clone(&volume))),
            ],
            threads: Vec::new(),
            renderers: vec![a, b],
            retired: Vec::new(),
            ring: Ring::new(TONE_RATE as usize * RING_SECONDS),
            sync: SyncBudget::new(4800, 0),
            source_rate: TONE_RATE,
            source_failure: Arc::new(WorkerFailure::default()),
        };
        let before = progress.load(Ordering::Relaxed);
        handle
            .reconcile_targets(&[Target {
                id: "a".into(),
                name: "a".into(),
                volume,
            }])
            .unwrap();
        assert!(b_stop.load(Ordering::Relaxed));
        assert!(!stop.load(Ordering::Relaxed));
        assert_eq!(handle.renderers[0].thread.thread().id(), a_thread);
        let deadline = Instant::now() + Duration::from_secs(1);
        while progress.load(Ordering::Relaxed) <= before && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(progress.load(Ordering::Relaxed) > before);
        handle.stop().unwrap();
    }
}
