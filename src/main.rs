#[cfg(not(windows))]
compile_error!("audio-multiplexer only supports Windows (WASAPI / Core Audio APIs).");

mod com;
mod devices;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::List => cmd_list(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_list() -> windows::core::Result<()> {
    let _com = com::ComGuard::new()?;
    let devices = devices::list_render_devices()?;

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
