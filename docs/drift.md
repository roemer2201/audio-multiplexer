# Clock-drift compensation

Status: implemented (Phase 5), tuning values are initial estimates.
Hardware measurements are still pending; the results section below must
be filled in once the engine has run on a real Windows machine with two
devices in different clock domains.

## Problem

Every audio device consumes samples at the pace of its own hardware
clock. Two "48 kHz" devices differ by some tens of ppm in practice, so
without compensation their outputs drift apart by several milliseconds
per minute and lose perceptual sync (see CLAUDE.md, "Known technical
risks").

## Approach: playback-lag controlled adaptive resampling

Each renderer uses IAudioClock::GetPosition and GetFrequency to measure
played output frames. Its QPC-correlated snapshot is extrapolated to the
time of the ring measurement. Submitted frames minus played frames includes
data already transferred from the endpoint buffer into the output pipeline.
Resampler output_delay is included as well. Converted to source-frame units:

    playback lag = unread source frames
                 + (submitted output - played output + resampler delay)
                   / current output-to-input ratio

This replaces the original equal-ring-fill approach: its rationale was
avoiding device-clock bookkeeping and using fill trends as an indirect
drift measurement. That simplification omitted the different output queues
and driver latencies and therefore could not establish playback alignment.

Each render thread owns a reader into the broadcast ring buffer. The
capture side produces frames at the source clock, the render side
consumes at the device clock. Therefore:

- device clock slower than nominal -> fill level rises
- device clock faster than nominal -> fill level falls

All outputs regulate this total lag to one shared budget. That budget is
100 ms of source headroom plus the largest initialized output pipeline
reserve. GetStreamLatency supplies a maximum used to reserve headroom, not
an extra term added to the measured queue (that would double-count latency).
Startup waits until all initial outputs have either prepared or failed.
Each reader then latches at budget minus its own pending output delay.
The shared budget only increases during a session, including target rejoin.

Nominal rate conversion and independent clock drift are handled by the
same ratio controller. Capture packet granularity and clock snapshots
still introduce measurement noise, so the controller remains slow.
Reported playback positions depend on the driver: external Bluetooth
transport, speaker DSP, and acoustic distance can introduce unreported
latency. Physical synchronization and the gains must be measured on real
hardware; software alignment is not an acoustic-sync guarantee.

References:
- https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclock-getposition
- https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclock-getfrequency
- https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-getstreamlatency
- https://docs.rs/rubato/3.0.0/rubato/struct.Async.html#method.output_delay

## Controller

Implemented in `src/render.rs` (`DriftController`):

- Total playback lag is sampled every render pass (typically every 10 ms)
  and smoothed with an EMA (alpha 0.1).
- Every 100 ms a PI controller computes a ratio correction from the
  normalized playback-lag error `(lag - budget) / budget`:
  correction = KP * error + integral, with
  integral += KI * error, clamped to +/- 0.005 (anti-windup).
- The total correction is clamped to +/- 2 percent and applied as
  `set_resample_ratio_relative(1.0 - correction, ramp = true)` on the
  rubato resampler (lag above budget -> consume input faster ->
  lower output/input ratio). The ramp smooths each step to keep it
  inaudible.
- Initial gains: KP = 0.05, KI = 0.005 per interval. Expected steady
  state: the integral term carries the constant ppm offset of the
  device pair, the proportional term handles transients.

Underruns (capture gaps, see loopback notes in `src/capture.rs`) pause
the controller: the render thread enters a rebuffering state, plays
silence, waits until enough input is available again, re-latches with its
own output-pipeline delay subtracted from the shared budget, and resets
the controller.
This prevents integral windup from gaps in the capture timeline.

## Measuring sync between two devices

1. Run the click generator on both devices under test:
   `audio-multiplexer test-tone --target A --target B --seconds 1800`
   (1 kHz burst, 10 ms long, once per second, sample-identical on all
   targets).
2. Record both outputs simultaneously: line-out of both devices into a
   stereo line-in (one device per channel), or two microphones placed
   at equal distance if only acoustic access is possible.
3. In an audio editor (or a small script), measure the offset between
   the burst positions of channel A and channel B at the start and at
   the end of the recording.
4. Drift = (offset_end - offset_start) / duration. The offset itself
   is the static latency difference (relevant later for the deferred
   per-device delay feature); Phase 5 only requires that it stays
   constant.

Budget (from PLAN.md): offset change < 5 ms over 1 hour without
audible artifacts. Refine after the first measurements.

## Results

TBD - to be filled in after the first run on real hardware:

- device pair tested (onboard vs USB recommended)
- steady-state drift correction (ppm) per device
- offset change over 30-60 min
- underruns/overruns observed
- controller gain adjustments, if any
