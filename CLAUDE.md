# CLAUDE.md

## Project: Windows Multi-Output Audio Router (working title)

An open-source application for Windows 10 and 11 that plays system audio
on multiple physical output devices simultaneously and in sync (same
room use case). Applications play into a silent virtual audio device;
this app captures that endpoint via WASAPI loopback and renders the
stream to N user-selected real devices.

## Status

Planning complete, architecture decided (2026-07-04). Implementation
not started.

## Decisions (settled)

- Architecture: Option C - user-mode fan-out engine + third-party
  virtual audio device as source (no own kernel driver).
- Language: Rust, using the windows-rs crate for WASAPI/Core Audio.
- UI: GUI with device selection (proposed framework: egui; final call
  during implementation).
- Primary use case: multiple devices in the same room, output must be
  perceptually synchronous. Per-device delay adjustment and clock-drift
  compensation are therefore core features, not extras.
- Distribution: open source on GitHub.

## Target platform

- Windows 10 (21H2 or later) and Windows 11, x64
- Audio API: WASAPI (Core Audio APIs), shared mode, event-driven
  buffers where supported

## Architecture

```
apps -> [virtual audio device, set as Windows default output, silent]
             |
             | WASAPI loopback capture (this app)
             v
      [ring buffer / mixer core, canonical format e.g. f32 48 kHz]
             |
   +---------+---------+-------- ... --------+
   v                   v                     v
[render thread 1]  [render thread 2]   [render thread N]
 resample            resample            resample
 drift comp          drift comp          drift comp
 per-dev delay       per-dev delay       per-dev delay
 per-dev volume      per-dev volume      per-dev volume
   |                   |                     |
 device 1            device 2             device N
```

Key points:

- The source is any selectable render endpoint captured via loopback.
  The app is driver-agnostic; it does not bundle or depend on a
  specific virtual driver at build time.
- Because ALL real outputs pass through this engine, relative latency
  between devices is fully controllable. Sync strategy: align all
  devices to the slowest one via per-device delay; compensate clock
  drift per device (adaptive resampling or sample insertion/drop).
- Rationale against a real device as loopback source: the source
  device is rendered directly by Windows and cannot be delayed, so
  copies always lag audibly behind it. Unsuitable for same-room sync.
- Rationale against an own kernel driver: virtual audio endpoints
  require a kernel-mode driver (SYSVAD/ACX); Microsoft's Rust driver
  platform (windows-drivers-rs) is early-stage and not recommended for
  production, and no ACX/audio Rust samples exist. Signing for public
  distribution would additionally require an EV certificate and
  attestation signing via the Microsoft Hardware Dev Center.

## Virtual device options (user-installed prerequisite, documented in README)

1. VirtualDrivers/Virtual-Audio-Driver (MIT, SYSVAD-derived,
   signed release available since 25.7.14, project labeled beta):
   https://github.com/VirtualDrivers/Virtual-Audio-Driver
2. VB-Cable (donationware, closed source, stable signed driver;
   user installs it themselves, no bundling):
   https://vb-audio.com/Cable/

## v1 feature scope

- Enumerate render endpoints (IMMDeviceEnumerator), pick source +
  N target devices in the GUI
- Per-device volume
- Clock-drift compensation per device (mandatory for sync; not a
  user-facing setting)
- Hot-plug handling (device arrival/removal via
  IMMNotificationClient)
- Config persistence (selected devices, volumes, delays) in
  %APPDATA%, TOML format
- Tray minimization (nice to have, may slip to v1.x)

## Non-goals for v1

- Per-device delay/latency configuration (deferred to a later
  version, if needed at all; mainly relevant for Bluetooth alignment)
- Own kernel driver
- Per-application routing
- Capture/microphone routing
- Audio effects/EQ

## Conventions (mandatory)

- ASCII-only characters in all code, scripts, and generated files.
  No Unicode symbols, no emojis, no typographic quotes.
- Code, comments, commit messages, and technical documentation in
  English. Communication with the project owner in German.
- Git workflow: all changes go into a branch named "claude".
  Before making changes, update from main/master first.
- Answers and design decisions must be backed by documentation
  (Microsoft Learn, API references). Speculation must be clearly
  labeled as such.
- Versioning: SemVer. License: MIT (TBC by owner).
- Rust: stable toolchain, cargo fmt + clippy clean, CI via GitHub
  Actions (windows-latest runner).

## Key references

- Core Audio APIs (WASAPI):
  https://learn.microsoft.com/en-us/windows/win32/coreaudio/wasapi
- Loopback recording:
  https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording
- windows-rs crate: https://github.com/microsoft/windows-rs
- SYSVAD sample (background/why no user-mode endpoint creation):
  https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/sample-audio-drivers
- ACX overview (background):
  https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-audio-class-extensions-overview

## Known technical risks

- Clock drift between independent devices desynchronizes output over
  minutes without active compensation; this is the hardest part of the
  project and should be prototyped first (CLI spike before GUI work).
- Devices with different native mix formats require per-device
  resampling from the canonical internal format.
- Bluetooth devices add large, variable latency; without the
  (deferred) per-device delay feature, mixing Bluetooth and wired
  devices in the same room will be audibly out of sync.
- Third-party virtual driver is an external dependency with its own
  signing/installation issues (see project issue trackers); the app
  must degrade gracefully if no virtual device is present (loopback of
  a real device still works, with the documented echo limitation).

## Suggested milestones

1. CLI spike: loopback capture -> single device render (validate
   latency, formats)
2. Multi-device render with drift compensation (measure sync with a
   test tone)
3. Per-device volume
4. GUI (device list, sliders, status), config persistence
5. Hot-plug, polish, packaging (GitHub release, cargo-dist or MSI)
