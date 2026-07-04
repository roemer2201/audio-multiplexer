# audio-multiplexer

An open-source application for Windows 10 and 11 that plays system
audio on multiple physical output devices simultaneously and in sync
(same-room use case).

Applications play into a silent virtual audio device; audio-multiplexer
captures that endpoint via WASAPI loopback and renders the stream to N
user-selected real devices, with per-device volume and clock-drift
compensation.

## Status

Early development. Currently implemented (CLI spike, phases 0-5 of the
roadmap):

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

Not yet implemented: GUI, config persistence, hot-plug handling.
See [PLAN.md](PLAN.md) for the full roadmap.

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

## Usage

```
audio-multiplexer list
audio-multiplexer record [--source <DEV>] [--seconds 10] [--out capture.wav]
audio-multiplexer play [--source <DEV>] --target <DEV> [--target <DEV> ...]
                       [--volume <DEV>=<0-100> ...]
audio-multiplexer test-tone --target <DEV> --target <DEV> [--seconds N]
```

`<DEV>` is an index from `list`, an endpoint ID, or an exact device
name. `--source` defaults to the Windows default render device (set
your silent virtual device as default so applications play into it).
The engine prints a status line per device every 5 seconds (buffer
fill, volume, applied drift correction, underruns); stop it with
Enter or `--seconds`.

While the engine runs, volumes can be changed per device without
clicks (short gain ramp): type `v <target#> <0-100>` and press Enter,
where `<target#>` is the index shown in the status lines.

Notes:

- Loopback only delivers audio while something is playing on the
  source endpoint; during silence the engine rebuffers.
- A target must not be the same device as the loopback source (this
  would create a feedback loop); the CLI rejects it.

## Building

Rust stable toolchain on Windows:

```
cargo build --release
```

CI runs `cargo fmt --check`, `cargo clippy -- -D warnings`, build and
tests on windows-latest.

## License

MIT, see [LICENSE](LICENSE).
