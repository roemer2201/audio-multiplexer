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

## Approach: fill-level controlled adaptive resampling

Design decision: drift is measured indirectly via the ring-buffer fill
level instead of correlating IAudioClock device positions with QPC.

Each render thread owns a reader into the broadcast ring buffer. The
capture side produces frames at the source clock, the render side
consumes at the device clock. Therefore:

- device clock slower than nominal -> fill level rises
- device clock faster than nominal -> fill level falls

Holding the fill level constant is equivalent to matching the device's
true consumption rate, which is exactly the drift compensation needed.
This also absorbs nominal rate mismatches (44.1 vs 48 kHz) in the same
mechanism, needs no extra APIs, and keeps all devices aligned to the
same buffering target (100 ms), which is the basis for mutual sync.

The trade-off versus IAudioClock/QPC correlation: fill-level control
reacts to buffering noise (packet granularity of the capture side, so
the loop must be slow), while IAudioClock would measure the clock
directly but adds QPC correlation, per-device position bookkeeping,
and its own jitter handling. If the fill-level approach turns out too
noisy on real hardware, IAudioClock-based measurement is the fallback
(revisit in that case).

## Controller

Implemented in `src/render.rs` (`DriftController`):

- The fill level is sampled every render pass (typically every 10 ms)
  and smoothed with an EMA (alpha 0.1).
- Every 100 ms a PI controller computes a ratio correction from the
  normalized fill error `(fill - target) / target`:
  correction = KP * error + integral, with
  integral += KI * error, clamped to +/- 0.005 (anti-windup).
- The total correction is clamped to +/- 2 percent and applied as
  `set_resample_ratio_relative(1.0 - correction, ramp = true)` on the
  rubato resampler (fill above target -> consume input faster ->
  lower output/input ratio). The ramp smooths each step to keep it
  inaudible.
- Initial gains: KP = 0.05, KI = 0.005 per interval. Expected steady
  state: the integral term carries the constant ppm offset of the
  device pair, the proportional term handles transients.

Underruns (capture gaps, see loopback notes in `src/capture.rs`) pause
the controller: the render thread enters a rebuffering state, plays
silence, waits until the target fill is available again, re-latches
exactly `target` frames behind the writer, and resets the controller.
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
