//! egui/eframe desktop UI.
//!
//! The GUI drives the engine exclusively through the `engine` module's
//! handle/volume interface; no audio calls happen on the UI thread. Config
//! changes are saved to %APPDATA% with a short debounce.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use eframe::egui;

use crate::com::ComGuard;
use crate::config::{self, Config, TargetConfig};
use crate::devices::{self, DeviceInfo};
use crate::engine::{self, EngineHandle, Target, Volume};
use crate::hotplug::HotplugWatcher;

const SAVE_DEBOUNCE: Duration = Duration::from_secs(1);
const REPAINT_INTERVAL: Duration = Duration::from_millis(250);

pub fn run_gui() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 560.0])
            .with_min_inner_size([420.0, 320.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Audio Multiplexer",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()) as Box<dyn eframe::App>)),
    )
    .map_err(|e| anyhow!("GUI failed: {e}"))
}

struct RunningEngine {
    handle: EngineHandle,
    source_id: String,
    /// Index-aligned with `handle.stats()`.
    target_ids: Vec<String>,
    volumes: Vec<Arc<Volume>>,
}

impl RunningEngine {
    fn volume_for(&self, device_id: &str) -> Option<&Arc<Volume>> {
        self.target_ids
            .iter()
            .position(|id| id == device_id)
            .map(|index| &self.volumes[index])
    }

    fn stats_for(&self, device_id: &str) -> Option<&Arc<engine::DeviceStats>> {
        self.target_ids
            .iter()
            .position(|id| id == device_id)
            .map(|index| &self.handle.stats()[index])
    }
}

struct App {
    /// Keeps COM alive on the UI thread for enumeration and the watcher.
    _com: Option<ComGuard>,
    devices: Vec<DeviceInfo>,
    config: Config,
    dirty_since: Option<Instant>,
    engine: Option<RunningEngine>,
    watcher: Option<HotplugWatcher>,
    last_error: Option<String>,
    /// Set when the engine stopped on its own (source failed/removed).
    engine_notice: Option<String>,
}

impl App {
    fn new() -> Self {
        let mut last_error = None;
        let com = match ComGuard::new() {
            Ok(guard) => Some(guard),
            Err(err) => {
                last_error = Some(format!("COM initialization failed: {err}"));
                None
            }
        };
        let config = config::load().unwrap_or_else(|err| {
            last_error = Some(format!("config not loaded: {err:#}"));
            Config::default()
        });
        let watcher = match HotplugWatcher::new() {
            Ok(watcher) => Some(watcher),
            Err(err) => {
                last_error = Some(format!("device notifications unavailable: {err}"));
                None
            }
        };
        let mut app = Self {
            _com: com,
            devices: Vec::new(),
            config,
            dirty_since: None,
            engine: None,
            watcher,
            last_error,
            engine_notice: None,
        };
        app.refresh_devices();
        app
    }

    fn refresh_devices(&mut self) {
        match devices::list_render_devices() {
            Ok(devices) => self.devices = devices,
            Err(err) => self.last_error = Some(format!("device enumeration failed: {err}")),
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty_since = Some(Instant::now());
    }

    fn save_if_due(&mut self, force: bool) {
        let due = match self.dirty_since {
            Some(since) => force || since.elapsed() >= SAVE_DEBOUNCE,
            None => false,
        };
        if !due {
            return;
        }
        match config::save(&self.config) {
            Ok(()) => self.dirty_since = None,
            Err(err) => {
                self.last_error = Some(format!("saving config failed: {err:#}"));
                // Avoid retrying every frame.
                self.dirty_since = None;
            }
        }
    }

    /// The endpoint the engine would capture: the configured source, or the
    /// current default render device.
    fn effective_source(&self) -> Option<&DeviceInfo> {
        match &self.config.source {
            Some(id) => self.devices.iter().find(|d| &d.id == id),
            None => self.devices.iter().find(|d| d.is_default),
        }
    }

    /// Target IDs the engine should run with right now: configured targets
    /// that are currently present and not the loopback source.
    fn desired_target_ids(&self) -> Vec<String> {
        let source_id = self.effective_source().map(|d| d.id.clone());
        self.config
            .targets
            .iter()
            .filter(|t| self.devices.iter().any(|d| d.id == t.id))
            .filter(|t| Some(&t.id) != source_id.as_ref())
            .map(|t| t.id.clone())
            .collect()
    }

    fn start_engine(&mut self) {
        self.engine_notice = None;
        let source = match self.effective_source() {
            Some(device) => device.clone(),
            None => {
                self.last_error =
                    Some("source device is not connected (or no default device)".to_string());
                return;
            }
        };
        let desired = self.desired_target_ids();
        if desired.is_empty() {
            self.last_error = Some("no connected target devices selected".to_string());
            return;
        }
        let mut targets = Vec::new();
        for id in &desired {
            let name = self
                .devices
                .iter()
                .find(|d| &d.id == id)
                .map(|d| d.name.clone())
                .unwrap_or_default();
            let volume = self.config.target(id).map(|t| t.volume).unwrap_or(100);
            targets.push(Target {
                id: id.clone(),
                name,
                volume: Volume::new(volume),
            });
        }
        match engine::start(
            engine::Source::Loopback {
                device_id: source.id.clone(),
                sample_rate: source.mix_format.sample_rate,
            },
            &targets,
        ) {
            Ok(handle) => {
                self.last_error = None;
                self.engine = Some(RunningEngine {
                    handle,
                    source_id: source.id,
                    target_ids: targets.iter().map(|t| t.id.clone()).collect(),
                    volumes: targets.iter().map(|t| Arc::clone(&t.volume)).collect(),
                });
            }
            Err(err) => self.last_error = Some(format!("engine start failed: {err:#}")),
        }
    }

    fn stop_engine(&mut self) {
        if let Some(running) = self.engine.take() {
            running.handle.stop();
        }
    }

    fn restart_engine_if_running(&mut self) {
        if self.engine.is_some() {
            self.stop_engine();
            self.start_engine();
        }
    }

    /// Called after device arrivals/removals: re-enumerate and adapt a
    /// running engine (rejoin replugged targets, drop removed ones, follow a
    /// changed default device when it is the implicit source).
    fn reconcile_after_device_change(&mut self) {
        self.refresh_devices();
        let Some(running) = &self.engine else {
            return;
        };
        let source_changed =
            self.effective_source().map(|d| d.id.as_str()) != Some(running.source_id.as_str());
        let mut desired = self.desired_target_ids();
        let mut active = running.target_ids.clone();
        desired.sort();
        active.sort();
        if source_changed || desired != active {
            self.restart_engine_if_running();
        }
    }

    /// Detects an engine that stopped on its own (source failure/removal).
    fn poll_engine_health(&mut self) {
        let died = self
            .engine
            .as_ref()
            .is_some_and(|running| !running.handle.is_running());
        if died {
            self.stop_engine();
            self.engine_notice =
                Some("engine stopped: the source failed or was removed".to_string());
        }
    }

    fn set_target_enabled(&mut self, device: &DeviceInfo, enabled: bool) {
        if enabled {
            if self.config.target(&device.id).is_none() {
                self.config.targets.push(TargetConfig {
                    id: device.id.clone(),
                    name: device.name.clone(),
                    volume: 100,
                    delay_ms: 0,
                });
            }
        } else {
            self.config.targets.retain(|t| t.id != device.id);
        }
        self.mark_dirty();
        self.restart_engine_if_running();
    }

    fn set_target_volume(&mut self, device_id: &str, percent: u8) {
        if let Some(target) = self.config.target_mut(device_id) {
            target.volume = percent;
            self.mark_dirty();
        }
        if let Some(running) = &self.engine
            && let Some(volume) = running.volume_for(device_id)
        {
            volume.set_percent(percent);
        }
    }

    fn source_section(&mut self, ui: &mut egui::Ui) {
        let devices = self.devices.clone();
        let selected_label = match &self.config.source {
            None => "Default render device".to_string(),
            Some(id) => devices
                .iter()
                .find(|d| &d.id == id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| format!("{id} (not connected)")),
        };
        let mut new_source: Option<Option<String>> = None;
        egui::ComboBox::from_label("Source (loopback capture)")
            .selected_text(selected_label)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.config.source.is_none(), "Default render device")
                    .clicked()
                {
                    new_source = Some(None);
                }
                for device in &devices {
                    let checked = self.config.source.as_deref() == Some(device.id.as_str());
                    let label = if device.is_default {
                        format!("{} (default)", device.name)
                    } else {
                        device.name.clone()
                    };
                    if ui.selectable_label(checked, label).clicked() {
                        new_source = Some(Some(device.id.clone()));
                    }
                }
            });
        if let Some(source) = new_source
            && source != self.config.source
        {
            self.config.source = source;
            self.mark_dirty();
            self.restart_engine_if_running();
        }
        if let Some(source) = self.effective_source() {
            ui.label(format!(
                "capturing: {} ({})",
                source.name, source.mix_format
            ));
        }
    }

    fn targets_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Target devices");
        let source_id = self.effective_source().map(|d| d.id.clone());
        let devices = self.devices.clone();
        for device in &devices {
            if Some(&device.id) == source_id.as_ref() {
                continue;
            }
            ui.push_id(&device.id, |ui| {
                self.target_row(ui, device);
            });
        }
        // Configured but currently unplugged targets stay listed (the config
        // entry is kept, see config.rs) so the user can still remove them.
        let missing: Vec<TargetConfig> = self
            .config
            .targets
            .iter()
            .filter(|t| !devices.iter().any(|d| d.id == t.id))
            .cloned()
            .collect();
        for target in missing {
            ui.push_id(&target.id, |ui| {
                ui.horizontal(|ui| {
                    let mut enabled = true;
                    let name = if target.name.is_empty() {
                        target.id.clone()
                    } else {
                        target.name.clone()
                    };
                    if ui
                        .checkbox(&mut enabled, format!("{name} (not connected)"))
                        .changed()
                        && !enabled
                    {
                        self.config.targets.retain(|t| t.id != target.id);
                        self.mark_dirty();
                    }
                });
            });
        }
    }

    fn target_row(&mut self, ui: &mut egui::Ui, device: &DeviceInfo) {
        ui.horizontal(|ui| {
            let mut enabled = self.config.target(&device.id).is_some();
            if ui.checkbox(&mut enabled, &device.name).changed() {
                self.set_target_enabled(device, enabled);
            }
            if enabled {
                let mut percent = self
                    .config
                    .target(&device.id)
                    .map(|t| t.volume)
                    .unwrap_or(100);
                if ui
                    .add(egui::Slider::new(&mut percent, 0..=100).suffix("%"))
                    .changed()
                {
                    self.set_target_volume(&device.id, percent);
                }
            }
        });
        if let Some(running) = &self.engine
            && let Some(stats) = running.stats_for(&device.id)
        {
            ui.label(format!(
                "        {} | fill {} ms | drift {:+} ppm | underruns {} | overruns {}",
                stats.state().as_str(),
                stats.fill_ms(),
                stats.drift_ppm(),
                stats.underruns(),
                stats.overruns()
            ));
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.watcher.as_ref().is_some_and(|w| w.take_changes()) {
            self.reconcile_after_device_change();
        }
        self.poll_engine_health();

        egui::Frame::central_panel(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Audio Multiplexer");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.engine.is_some() {
                        if ui.button("Stop").clicked() {
                            self.stop_engine();
                        }
                        ui.colored_label(egui::Color32::from_rgb(64, 160, 64), "running");
                    } else {
                        if ui.button("Start").clicked() {
                            self.start_engine();
                        }
                        ui.label("stopped");
                    }
                    if self.watcher.is_none() && ui.button("Refresh devices").clicked() {
                        self.refresh_devices();
                    }
                });
            });
            ui.separator();

            if let Some(error) = self.last_error.clone() {
                ui.horizontal(|ui| {
                    ui.colored_label(egui::Color32::from_rgb(200, 80, 80), &error);
                    if ui.small_button("x").clicked() {
                        self.last_error = None;
                    }
                });
            }
            if let Some(notice) = self.engine_notice.clone() {
                ui.horizontal(|ui| {
                    ui.colored_label(egui::Color32::from_rgb(200, 150, 60), &notice);
                    if ui.small_button("x").clicked() {
                        self.engine_notice = None;
                    }
                });
            }
            if self.devices.is_empty() {
                ui.label("No active render devices found. Install/enable a device and try again.");
            }

            self.source_section(ui);
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.targets_section(ui);
            });
        });

        self.save_if_due(false);
        // Keep status lines and hotplug polling fresh.
        ui.ctx().request_repaint_after(REPAINT_INTERVAL);
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_engine();
        self.save_if_due(true);
    }
}
