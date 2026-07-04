#[cfg(not(windows))]
compile_error!("audio-multiplexer only supports Windows (WASAPI / Core Audio APIs).");

mod capture;
mod com;
mod devices;
mod engine;
mod render;
mod ring;
mod tone;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::devices::DeviceInfo;

#[derive(Parser)]
#[command(
    name = "audio-multiplexer",
    version,
    about = "Plays Windows system audio on multiple output devices simultaneously and in sync"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List all active audio render endpoints
    List,
    /// Record the loopback stream of a render endpoint into a WAV file
    Record {
        /// Source endpoint (index from `list`, endpoint ID, or name);
        /// defaults to the default render device
        #[arg(long)]
        source: Option<String>,
        /// Recording duration in wall-clock seconds
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        /// Output WAV file path
        #[arg(long, default_value = "capture.wav")]
        out: PathBuf,
    },
    /// Capture a source endpoint via loopback and play it on target devices
    Play {
        /// Source endpoint (index from `list`, endpoint ID, or name);
        /// defaults to the default render device
        #[arg(long)]
        source: Option<String>,
        /// Target endpoint (repeatable)
        #[arg(long = "target", required = true)]
        targets: Vec<String>,
        /// Initial volume for a target device (repeatable, default 100)
        #[arg(long = "volume", value_name = "DEV=PERCENT")]
        volumes: Vec<String>,
        /// Stop automatically after this many seconds
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// Play a periodic click pattern on target devices for sync measurement
    TestTone {
        /// Target endpoint (repeatable)
        #[arg(long = "target", required = true)]
        targets: Vec<String>,
        /// Initial volume for a target device (repeatable, default 100)
        #[arg(long = "volume", value_name = "DEV=PERCENT")]
        volumes: Vec<String>,
        /// Stop automatically after this many seconds
        #[arg(long)]
        seconds: Option<u64>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::List => cmd_list(),
        Command::Record {
            source,
            seconds,
            out,
        } => {
            let devices = enumerate()?;
            let source = resolve_source(source.as_deref(), &devices)?;
            println!("Source: {}", source.name);
            capture::record_to_wav(&source.id, seconds, &out)
        }
        Command::Play {
            source,
            targets,
            volumes,
            seconds,
        } => {
            let devices = enumerate()?;
            let source = resolve_source(source.as_deref(), &devices)?;
            let targets = resolve_targets(&targets, &devices)?;
            apply_volume_args(&volumes, &targets, &devices)?;
            for target in &targets {
                if target.id == source.id {
                    bail!(
                        "target '{}' is the loopback source; playing onto the captured \
                         endpoint would create a feedback loop",
                        target.name
                    );
                }
            }
            println!("Source: {} ({})", source.name, source.mix_format);
            print_targets(&targets);
            engine::run(
                engine::Source::Loopback {
                    device_id: source.id.clone(),
                    sample_rate: source.mix_format.sample_rate,
                },
                targets,
                seconds,
            )
        }
        Command::TestTone {
            targets,
            volumes,
            seconds,
        } => {
            let devices = enumerate()?;
            let targets = resolve_targets(&targets, &devices)?;
            apply_volume_args(&volumes, &targets, &devices)?;
            print_targets(&targets);
            engine::run(engine::Source::Tone, targets, seconds)
        }
    }
}

fn enumerate() -> Result<Vec<DeviceInfo>> {
    let _com = com::ComGuard::new()?;
    Ok(devices::list_render_devices()?)
}

fn cmd_list() -> Result<()> {
    let devices = enumerate()?;
    if devices.is_empty() {
        println!("No active render devices found.");
        return Ok(());
    }
    println!("Active render devices:");
    println!();
    for (index, device) in devices.iter().enumerate() {
        let default_marker = if device.is_default { " (default)" } else { "" };
        println!("[{index}] {}{default_marker}", device.name);
        println!("    id:         {}", device.id);
        println!("    mix format: {}", device.mix_format);
        println!();
    }
    Ok(())
}

/// Resolves a device selector: an index from `list`, an endpoint ID, or an
/// exact friendly name. Without a selector the default endpoint is used.
fn resolve_device<'a>(selector: &str, devices: &'a [DeviceInfo]) -> Result<&'a DeviceInfo> {
    if let Ok(index) = selector.parse::<usize>() {
        return devices.get(index).with_context(|| {
            format!(
                "device index {index} is out of range (0..{})",
                devices.len().saturating_sub(1)
            )
        });
    }
    devices
        .iter()
        .find(|d| d.id == selector || d.name == selector)
        .with_context(|| format!("no active render device matches '{selector}'"))
}

fn resolve_source<'a>(selector: Option<&str>, devices: &'a [DeviceInfo]) -> Result<&'a DeviceInfo> {
    match selector {
        Some(selector) => resolve_device(selector, devices),
        None => devices
            .iter()
            .find(|d| d.is_default)
            .context("no default render device found; specify --source"),
    }
}

fn resolve_targets(selectors: &[String], devices: &[DeviceInfo]) -> Result<Vec<engine::Target>> {
    let mut targets = Vec::with_capacity(selectors.len());
    for selector in selectors {
        let device = resolve_device(selector, devices)?;
        if targets.iter().any(|t: &engine::Target| t.id == device.id) {
            bail!("target '{}' was specified more than once", device.name);
        }
        targets.push(engine::Target {
            id: device.id.clone(),
            name: device.name.clone(),
            volume: engine::Volume::new(100),
        });
    }
    Ok(targets)
}

/// Applies `--volume <DEV>=<PERCENT>` arguments; the device part accepts the
/// same selectors as `--target` and must name one of the chosen targets.
fn apply_volume_args(
    volume_args: &[String],
    targets: &[engine::Target],
    devices: &[DeviceInfo],
) -> Result<()> {
    for arg in volume_args {
        let (selector, percent) = arg
            .rsplit_once('=')
            .with_context(|| format!("--volume '{arg}' must have the form <device>=<percent>"))?;
        let percent: u8 = percent
            .parse()
            .ok()
            .filter(|p| *p <= 100)
            .with_context(|| format!("--volume '{arg}': percent must be between 0 and 100"))?;
        let device = resolve_device(selector, devices)?;
        let target = targets
            .iter()
            .find(|t| t.id == device.id)
            .with_context(|| format!("--volume device '{}' is not a target", device.name))?;
        target.volume.set_percent(percent);
    }
    Ok(())
}

fn print_targets(targets: &[engine::Target]) {
    println!("Targets:");
    for target in targets {
        println!("  - {} (volume {}%)", target.name, target.volume.percent());
    }
}
