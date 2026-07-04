# audio-multiplexer

An open-source application for Windows 10 and 11 that plays system
audio on multiple physical output devices simultaneously and in sync
(same-room use case).

Applications play into a silent virtual audio device; audio-multiplexer
captures that endpoint via WASAPI loopback and renders the stream to N
user-selected real devices, with per-device volume and clock-drift
compensation.

## Status

Early development. Currently implemented:

- `audio-multiplexer list` - enumerate all active render endpoints
  with ID, friendly name, default marker, and shared-mode mix format

See [PLAN.md](PLAN.md) for the full implementation roadmap.

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
```

Lists all active audio render endpoints. More subcommands (record,
play) will follow per the roadmap.

## Building

Rust stable toolchain on Windows:

```
cargo build --release
```

CI runs `cargo fmt --check`, `cargo clippy -- -D warnings`, build and
tests on windows-latest.

## License

MIT, see [LICENSE](LICENSE).
