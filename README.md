# audio-multiplexer

An open-source application for Windows 10 and 11 that plays system
audio on multiple physical output devices simultaneously and in sync
(same-room use case).

Applications play into a silent virtual audio device; audio-multiplexer
captures that endpoint via WASAPI loopback and renders the stream to N
user-selected real devices, with per-device volume and clock-drift
compensation.

## Status

Feature-complete for v1 (phases 0-10 of [PLAN.md](PLAN.md)):

- GUI (default when started without arguments): source picker, target
  checkboxes with volume sliders, live status per device, hot-plug
  handling (replugged devices rejoin automatically), settings
  persisted to `%APPDATA%\audio-multiplexer\config.toml`
- `list` - enumerate all active render endpoints with ID, friendly
  name, default marker, and shared-mode mix format
- `record` - capture a render endpoint via WASAPI loopback into a WAV
  file (verification tool)
- `play` - capture a source endpoint and play it on one or more
  target devices simultaneously, with per-device clock-drift
  compensation (see [docs/drift.md](docs/drift.md)) and per-device
  volume (adjustable at runtime, click-free ramping)
- `test-tone` - play a periodic click pattern on multiple devices to
  measure their relative drift

Runtime validation on real hardware (drift measurement, see
docs/drift.md) is still pending before a v1.0.0 release is tagged.

Deferred to a later version: per-device delay configuration (the
config file already reserves `delay_ms`), tray minimization, a
separate windowed launcher (starting the GUI from Explorer currently
also opens a console window).

## Requirements

- Windows 10 (21H2 or later) or Windows 11, x64
- A virtual audio device to use as the silent source endpoint
  (user-installed, not bundled). Tested options:
  1. [VirtualDrivers/Virtual-Audio-Driver](https://github.com/VirtualDrivers/Virtual-Audio-Driver)
     (MIT, signed release available, project labeled beta)
  2. [VB-Cable](https://vb-audio.com/Cable/) (donationware, closed
     source, stable signed driver)

Without a virtual device, loopback capture of a real device still
works, but that device is also rendered directly by Windows and cannot
be delayed, so the copies will audibly lag behind it (echo limitation).

## Setup

1. Install one of the virtual audio devices above.
2. Set the virtual device as the Windows default output (Settings >
   System > Sound), so applications play into it silently.
3. Start `audio-multiplexer.exe`: pick the virtual device as source
   (or leave "Default render device"), tick the real output devices,
   press Start, and adjust the sliders.

Selections and volumes are saved automatically and restored on the
next start.

## CLI usage

```
audio-multiplexer                 (starts the GUI; same as "gui")
audio-multiplexer list
audio-multiplexer record [--source <DEV>] [--seconds 10] [--out capture.wav]
audio-multiplexer play [--source <DEV>] --target <DEV> [--target <DEV> ...]
                       [--volume <DEV>=<0-100> ...]
audio-multiplexer play            (restores the saved configuration)
audio-multiplexer test-tone --target <DEV> --target <DEV> [--seconds N]
```

`<DEV>` is an index from `list`, an endpoint ID, or an exact device
name. `--source` defaults to the Windows default render device.
The engine prints a status line per device every 5 seconds (buffer
fill, volume, applied drift correction, underruns); stop it with
Enter or `--seconds`.

While the engine runs, volumes can be changed per device without
clicks (short gain ramp): type `v <target#> <0-100>` and press Enter,
where `<target#>` is the index shown in the status lines.

`play` with explicit `--target` arguments saves the session to the
config file; `play` without targets restores it (unplugged devices
are skipped with a warning but stay configured).

Notes:

- Loopback only delivers audio while something is playing on the
  source endpoint; during silence the engine rebuffers.
- A target must not be the same device as the loopback source (this
  would create a feedback loop); the CLI rejects it and the GUI
  excludes the source from the target list.

## Building

Rust stable toolchain on Windows:

```
cargo build --release
```

CI runs `cargo fmt --check`, `cargo clippy -- -D warnings`, build and
tests on windows-latest. Pushing a tag `v*` builds a release zip and
drafts a GitHub release (see `.github/workflows/release.yml`).

## License

MIT, see [LICENSE](LICENSE).
