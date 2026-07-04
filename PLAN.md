# Implementation Plan

Ordered programming plan for the Windows multi-output audio router.
It refines the milestones in CLAUDE.md into concrete, sequential work
packages. Each phase has a goal, tasks, and a definition of done (DoD).
The order is risk-driven: the hardest and most uncertain parts (WASAPI
loopback, format handling, clock-drift compensation) are validated in a
CLI spike before any GUI work starts.

Rationale for the ordering: clock drift between independent audio
devices is the main technical risk (see CLAUDE.md, "Known technical
risks"). If drift compensation turns out to be infeasible with
acceptable quality, the project approach must be revisited; therefore
everything GUI- and comfort-related comes after that proof point.

References used throughout:

- WASAPI: https://learn.microsoft.com/en-us/windows/win32/coreaudio/wasapi
- Loopback recording: https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording
- IMMDeviceEnumerator: https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nn-mmdeviceapi-immdeviceenumerator
- IAudioClient: https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nn-audioclient-iaudioclient
- IAudioClock: https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nn-audioclient-iaudioclock
- IMMNotificationClient: https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nn-mmdeviceapi-immnotificationclient
- windows-rs: https://github.com/microsoft/windows-rs

## Phase 0: Project scaffolding

Goal: a compilable, CI-checked Rust skeleton.

Tasks:

1. `cargo init` (binary crate, name TBD), Rust stable toolchain,
   `rust-toolchain.toml`.
2. Add `windows` crate with the required feature flags
   (`Win32_Media_Audio`, `Win32_System_Com`, `Win32_Foundation`,
   `Win32_Devices_FunctionDiscovery` for endpoint friendly names).
3. `.gitignore`, `LICENSE` (MIT, to be confirmed by owner),
   `rustfmt.toml` if defaults are not used.
4. GitHub Actions CI on `windows-latest`: `cargo fmt --check`,
   `cargo clippy -- -D warnings`, `cargo build`, `cargo test`.
5. README skeleton: what the app does, prerequisite virtual driver
   options (VirtualDrivers/Virtual-Audio-Driver, VB-Cable), install
   pointers.

DoD: CI is green on a commit that compiles a hello-world binary.

## Phase 1: Device enumeration (CLI)

Goal: list all active render endpoints from Rust.

Tasks:

1. COM initialization (`CoInitializeEx`, MTA) and error handling
   strategy (thin wrapper around `windows::core::Result`).
2. Enumerate render endpoints via `IMMDeviceEnumerator::
   EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)`.
3. Print per device: endpoint ID, friendly name (property store,
   `PKEY_Device_FriendlyName`), default-device marker, mix format
   (`IAudioClient::GetMixFormat`).
4. CLI argument parsing (e.g. `clap`): `list` subcommand.

DoD: `app list` prints all active render devices with name, ID and
mix format on a real Windows 10/11 machine.

## Phase 2: Loopback capture (CLI)

Goal: capture audio from a selectable render endpoint via WASAPI
loopback and prove the samples are correct.

Tasks:

1. Open the selected endpoint in shared mode with
   `AUDCLNT_STREAMFLAGS_LOOPBACK`.
2. Event-driven capture where supported; note: loopback capture
   events require the documented workaround (a loopback client alone
   does not get events; pair it with a render client on the same
   device or poll). Validate against Microsoft's loopback docs during
   implementation.
3. Capture thread with `IAudioCaptureClient::GetBuffer`, handling
   `AUDCLNT_BUFFERFLAGS_SILENT` and discontinuity flags.
4. `record` subcommand: capture N seconds into a WAV file (e.g. via
   `hound`) for offline verification.

DoD: playing music on the source endpoint and running
`app record --seconds 10` produces a WAV file that sounds identical.

## Phase 3: Single-device passthrough (CLI spike, milestone 1)

Goal: end-to-end audio path capture -> ring buffer -> render on one
target device; validate latency and format conversion.

Tasks:

1. Canonical internal format: f32, 48 kHz, channel count TBD
   (start stereo). Convert captured frames into it.
2. Lock-free SPSC ring buffer between capture and render thread
   (e.g. `ringbuf` crate or own implementation; decide and document).
3. Render thread: shared mode, event-driven
   (`AUDCLNT_STREAMFLAGS_EVENTCALLBACK`), pre-fill, underrun handling
   (fill with silence, count occurrences).
4. Static resampling when the device mix format differs from the
   canonical format (e.g. `rubato` crate); no drift handling yet.
5. `play` subcommand: `app play --source <id> --target <id>`.
6. Log end-to-end latency estimate and underrun/overrun counters.

DoD: stable passthrough for 30+ minutes without underruns on one
device; audible latency roughly in the expected shared-mode range
(tens of milliseconds).

## Phase 4: Multi-device fan-out

Goal: one capture, N independent render threads.

Tasks:

1. Replace the SPSC buffer with a broadcast structure: one writer,
   N readers with independent read positions (SPMC ring with per-
   reader cursors).
2. One render thread per target device, each with its own format
   conversion/resampler instance and its own error/underrun counters.
3. Per-device buffer depth: all devices aligned to a common target
   fill level so later delay alignment is possible.
4. `play` accepts multiple `--target` arguments.
5. Graceful per-device failure: one failing device must not stop the
   others.

DoD: simultaneous output on 2-3 physical devices; engine survives
one device being disconnected mid-play (stream stops, others go on).

## Phase 5: Clock-drift compensation (milestone 2, highest risk)

Goal: outputs stay perceptually synchronous over hours, not just
minutes.

Tasks:

1. Drift measurement per device: compare device position
   (`IAudioClock::GetPosition`, QPC-correlated) and/or ring-buffer
   fill level trend against the capture clock.
2. Control loop per device: slow PI-style controller adjusting an
   adaptive resampling ratio (e.g. `rubato` with variable ratio);
   fallback strategy sample insert/drop for very small corrections.
3. Anti-windup and smoothing so corrections stay inaudible.
4. Measurement tooling: `app test-tone` subcommand rendering a
   click/impulse pattern; record two devices' outputs (line-in or
   microphone) and measure offset drift over 30-60 minutes.
5. Document measured residual drift and correction behavior in
   `docs/drift.md`.

DoD: with two devices of different clock domains (e.g. onboard +
USB), measured offset stays within a defined budget (target:
< 5 ms deviation over 1 hour, refine after first measurements)
without audible artifacts.

Decision gate: only proceed to GUI work when this phase meets its
DoD. If it cannot be met, revisit architecture before investing in UI.

## Phase 6: Per-device volume (milestone 3)

Goal: independent volume per target device.

Tasks:

1. Per-device gain stage in the render path (linear gain applied to
   f32 samples; dB mapping for the UI later).
2. Smooth ramping on changes (short linear ramp) to avoid zipper
   noise.
3. CLI: `--volume <id>=<0..100>` and runtime change via simple stdin
   commands (prepares the control-channel design for the GUI).

DoD: volumes are independently adjustable at runtime without clicks.

## Phase 7: Engine/control separation and config persistence

Goal: clean internal API so a GUI can drive the engine; settings
survive restarts.

Tasks:

1. Refactor into an `engine` module (or crate in a workspace) with a
   command/status channel interface (start/stop, set targets, set
   volume; status: levels, underruns, drift metrics).
2. Config file in `%APPDATA%/<AppName>/config.toml` (via `dirs`
   crate): source endpoint ID, target endpoint IDs, per-device
   volume, per-device delay field reserved for later.
3. Load on start, save on change (debounced) and on exit; tolerate
   missing/stale device IDs (keep entry, mark unavailable).

DoD: restarting the CLI restores the previous session's devices and
volumes.

## Phase 8: GUI (milestone 4)

Goal: usable desktop UI replacing the CLI as primary interface
(CLI subcommands remain for diagnostics).

Tasks:

1. Confirm framework choice egui/eframe (decision record in
   `docs/decisions.md`); evaluate only if a blocker appears.
2. Views: source device picker, target device checklist, per-device
   volume slider, per-device status (active, underruns, drift),
   engine start/stop.
3. GUI talks to the engine exclusively via the Phase 7 channel
   interface; no audio calls from the UI thread.
4. Error surfaces: missing virtual device hint (with README link),
   device open failures, engine panics.

DoD: a user can select source and targets, start the engine, and
adjust volumes entirely via the GUI; settings persist.

## Phase 9: Hot-plug handling (milestone 5, part 1)

Goal: react to device arrival/removal/default changes at runtime.

Tasks:

1. Implement `IMMNotificationClient` (device added/removed/state
   changed/default changed) and forward events into the engine.
2. Removal of an active target: stop that render thread, mark device
   unavailable in UI, keep config entry.
3. Arrival of a configured device: offer/perform automatic rejoin.
4. Source device removal: stop capture, surface a clear status.

DoD: unplugging and replugging a USB device during playback is
handled without crash; replug resumes output on that device.

## Phase 10: Polish and packaging (milestone 5, part 2)

Goal: releasable v1.

Tasks:

1. Tray minimization (nice to have; may slip to v1.x per CLAUDE.md).
2. Logging (e.g. `tracing`) with a file sink for support cases.
3. README: full setup guide including virtual driver installation
   and the echo limitation when looping back a real device.
4. Packaging: GitHub release via cargo-dist or MSI (decide in this
   phase); version v1.0.0 per SemVer.
5. Final pass: `cargo fmt`, `clippy` clean, CI green, license files.

DoD: a tagged GitHub release a user can download and run following
only the README.

## Explicitly out of scope for v1 (from CLAUDE.md)

- Per-device delay/latency configuration (config field is reserved,
  UI deferred)
- Own kernel driver, per-application routing, capture/microphone
  routing, effects/EQ

## Suggested branch/PR slicing

One PR per phase (Phases 0-2 may be combined if small). Each PR must
be fmt/clippy clean and keep CI green. Measurements and decisions go
into `docs/` alongside the code that produced them.
