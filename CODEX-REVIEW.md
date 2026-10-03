# Code Review: audio-multiplexer

Date: 2026-10-03
Reviewed branch: claude/programming-plan-y17ykw
Reviewed commit: 7d11f6e62e3da484a2726adfc6eebe0d10c6e6f1
Reviewer: Codex

## Scope and validation limits

Reviewed all 11 Rust modules, Cargo manifest and lockfile, CI/release
workflows, installer, README, implementation plan, and drift documentation.
The default branch main contains only README.md and CLAUDE.md, so this
review covers the only implementation branch. It is seven commits ahead
of main and zero commits behind it.

This is a static review with checks against Microsoft, Rust, and the
locked rubato 3.0.0 documentation/source. No Windows audio device or Rust
toolchain is available in the review environment. Build, clippy, cargo
tests, installer execution, audio-quality measurements, and actual
hot-plug reproduction were NOT run. The GitHub Actions runs API returned
zero runs at review time. That does not prove a build failure, but it
provides no CI evidence of a passing Windows build either.

There are 12 existing unit tests: four for the ring, four for gain
ramping, two for configuration serialization, and two for tone generation.
None exercises drift convergence, endpoint latency alignment, engine
failure propagation, or GUI hot-plug state transitions.

Priority: P1 = fix before claiming the central audio-quality/synchronization
behavior; P2 = functional or lifecycle bug to fix before a stable release.
Hardware-dependent audible severity is explicitly distinguished below
from the missing behavior visible in the code.

## Findings

### R1 [P1] Equal ring fill does not align the actual playback positions

Location: src/render.rs:148-160, 206-222; src/engine.rs:24-27, 238-239.

Every reader aims for the same 100 ms of unread source frames. The
controller samples ring fill after reading and submitting a packet.
However, the reader position is the input submitted to the resampler,
not the sample currently audible on the physical endpoint. Samples
already buffered in the resampler, queued in WASAPI, and delayed in
the device are excluded from the controlled value. GetCurrentPadding
is used only to size the next packet; it never contributes to the
synchronization error.

With endpoints having different output queues/latencies, equal reader
fill can coexist with a persistent difference in audible playback.
For example, an additional 30 ms in one output path is not removed by
holding both ring readers at 100 ms. This is an illustrative latency
budget, not a hardware measurement.

This is separate from the explicitly deferred user-adjustable delay
setting: the application also lacks automatic baseline alignment.
The controller may compensate consumption-rate drift, but that alone
does not establish the README's same-room synchronous-output behavior.
Independent seek-to-latest recovery can also shift relative alignment.

Correction: establish a common playback timeline and account for queued
output and resampler delay. Use device-clock/QPC correlation or a
validated equivalent to compare playback positions. Stream-latency
information can help build a budget, but maximum stream latency alone
is not a complete acoustic calibration, especially for Bluetooth.
If alignment is deferred, narrow the advertised behavior and do not
state that identical ring fill keeps physical outputs synchronized.

Verification: two outputs with deliberately different queue depths;
simultaneous impulse recordings at start, after one hour, and after an
underrun/rejoin. Measure initial offset and offset change separately.

References:
- [GetCurrentPadding](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-getcurrentpadding)
- [GetStreamLatency](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-getstreamlatency)
- [rubato output_delay](https://docs.rs/rubato/3.0.0/rubato/struct.Async.html#method.output_delay)

### R2 [P1] Downsampling uses interpolation without an anti-aliasing filter

Location: src/render.rs:109-117.

All rate conversions use Async::new_poly with Septic interpolation.
rubato 3.0.0 explicitly documents that this resampler has no
anti-aliasing filter. Interpolation degree does not replace the low-pass
filter needed before downsampling.

A 48 kHz source played on a 44.1 kHz endpoint can fold source content
above 22.05 kHz into the output band. A stronger diagnostic case is a
96 kHz source containing a 30 kHz tone rendered at 48 kHz: unfiltered
decimation produces an 18 kHz alias. That frequency relation is
analytical; the application was not executed for this review.

Correction: use a suitable anti-aliasing resampler/filter for downward
rate conversion, with its cutoff designed for the nominal ratio and
allowed drift adjustment. Do not assume default sinc settings are
correct for every ratio without validating their bandwidth.

Verification: measure passband and alias suppression for 48 -> 44.1,
96 -> 48, and supported extreme rate pairs, including ratio corrections.

References:
- [rubato 3.0.0 Async documentation](https://docs.rs/rubato/3.0.0/rubato/struct.Async.html#interpolation)
- [Locked-version implementation](https://github.com/HEnquist/rubato/blob/v3.0.0/src/asynchro.rs)

### R3 [P2] Reseeking resets the controller but retains stale resampler state

Location: src/render.rs:157-160, 175-187.

After an underrun or overwrite, the reader moves to a new source
position and DriftController resets. The resampler is neither reset nor
returned explicitly to its base ratio. Its interpolation history and
previous ratio therefore remain tied to the old source position.

When playback resumes, processing can interpolate across unrelated
old/new samples, and the old correction remains active until a later
controller update. After a long source gap or a jump over overwritten
audio this can create a discontinuity and an inconsistent recovery.
The controller reset does not reset rubato.

Correction: reset the resampler together with a timeline reseek and
restore the intended ratio consistently; apply an appropriate restart
fade if click-free recovery is required. Recompute chunk/input sizes
after resetting, because rubato reset restores the maximum chunk size.

Verification: constant/impulse signals separated by a source pause or
forced overwrite, checking that no old tail is replayed and the ratio
returns to the intended recovery value.

Reference:
- [rubato reset contract](https://docs.rs/rubato/3.0.0/rubato/struct.Async.html#method.reset)
- [rubato reset implementation](https://github.com/HEnquist/rubato/blob/v3.0.0/src/asynchro.rs#L606-L628)

### R4 [P2] Replugging the last target does not automatically resume playback

Location: src/gui.rs:170-173, 210-239.

Start with one connected target, then unplug it. Reconciliation sees
a changed target set, removes the RunningEngine through stop_engine,
and calls start_engine. With no connected targets, start_engine returns
without creating an engine. On replug, reconciliation immediately
returns because self.engine is None.

The same problem occurs if all configured targets are removed together.
The configuration remains selected, but playback requires a manual
Start, contradicting the documented automatic rejoin.

Correction: keep user intent to run independently of whether there is
an active EngineHandle. Reconcile a waiting session when targets return,
and clear that intent on an explicit user Stop. Source removal can
retain its separately defined stop policy.

Verification: unplug/replug the only target; unplug all targets and
restore one; explicitly press Stop while waiting and verify that a later
replug does not start playback.

### R5 [P2] Changing one target restarts every healthy output

Location: src/gui.rs:216-239, 255-269.

With outputs A and B running, unplugging B triggers a full engine
shutdown and restart for A. Adding a configured target or changing a
target checkbox takes the same path. The source and all healthy
renderers are discarded, then their replacement readers rebuffer.

Thus a target failure interrupts unaffected outputs in the GUI, even
though the engine module and Phase 9 promise per-device isolation.
stop_engine also joins the workers directly on the GUI thread; a
renderer waiting for its event can take up to its 2000 ms timeout to
observe the stop request.

Correction: support adding/removing individual render workers while
keeping the source/ring/healthy renderers alive. Perform potentially
blocking worker shutdown outside the GUI update, and provide a wakeup
mechanism for stop requests.

Verification: record output A continuously while unplugging/replugging
B; A must not acquire a gap or reset its synchronization state, and
the GUI must remain responsive.

### R6 [P2] --volume is silently ignored when restoring a saved session

Location: src/main.rs:135-142.

apply_volume_args is called only when explicit --target arguments are
present. The restore branch never applies or validates volume_args.
For a saved target A, play --volume A=20 therefore uses the saved gain;
even malformed --volume arguments pass unnoticed on that path.

Correction: resolve/restore the targets first, then apply volume
arguments in the common path before starting the engine. Define
separately whether a CLI override should overwrite saved settings.

Verification: restore a saved session with a valid volume override,
an invalid percentage, and a volume selector naming a non-target.

### R7 [P2] Source failures produce a successful CLI exit status

Location: src/engine.rs:277-288, 335-340; src/main.rs:85-92.

If source opening, capture startup, or capture draining fails, the
source worker prints the error and sets stop. Its JoinHandle returns
(), EngineHandle shutdown discards join results, and engine::run
unconditionally returns Ok(()). main therefore exits successfully
despite a source failure, including a startup that captured no audio.

External scripts cannot distinguish a completed session from a failed
one. The GUI receives only a generic source-failed notice, with the
actual error confined to stderr.

Correction: preserve worker outcomes and stop reasons in the engine
control/status API; propagate source failures through engine::run to
main. Retain per-device failure isolation when some targets still work,
and define the terminal outcome when all outputs fail.

Verification: inject source-open, source-start and source-drain errors;
each must yield a clear failure result and nonzero CLI exit. Normal
Enter/timed stop must still succeed.

### R8 [P2] A thread-spawn error leaves previously started workers detached

Location: src/engine.rs:242-290.

EngineHandle, whose Drop requests stop and joins workers, is constructed
only after all workers have been spawned. If a later renderer spawn
fails, or source spawning fails, ? returns early and drops the local
Vec<JoinHandle<()>> without setting stop.

Dropping JoinHandle detaches a thread. Earlier renderers retain their
Arc stop flags with value false and can keep running/rebuffering with
open audio clients after start reports failure. In the GUI process,
retrying Start can accumulate workers that have no controllable handle.

This requires an OS thread-creation failure; it is a resource-pressure
path, not the normal startup path.

Correction: give partial startup an RAII owner immediately, or perform
explicit rollback that sets stop and joins every successfully spawned
worker on any spawn error.

Verification: inject failure at the second renderer spawn and at source
spawn; assert all prior workers exit and no device stream remains open.

Reference:
- [Rust JoinHandle drop semantics](https://doc.rust-lang.org/std/thread/struct.JoinHandle.html)

## Further improvements and conditional risks

These are not included in the eight primary findings.

- Rapid unplug/replug can be coalesced by take_changes into an unchanged
  final ID set. Reconciliation compares IDs, not worker health. If an
  old renderer already failed, it can stay failed even though its device
  is available again. Reconcile availability and worker state together.
- Endpoint format/property changes are ignored by
  OnPropertyValueChanged. Invalidated streams whose endpoint IDs remain
  unchanged need recovery beyond set comparison. Subscribe to relevant
  format/session invalidation and expose the actual worker error.
- Render-event timeouts only continue the loop. If an invalidated device
  stops signaling, the CLI renderer can retain an obsolete Running
  state without ever checking an API for invalidation. Add bounded health
  checks; one timeout by itself need not mean device failure.
- Capture conversion keeps only the first two channels. A 5.1 source
  loses center/surround content instead of being downmixed; center-only
  dialogue can disappear. Either implement channel-mask-aware downmixing
  or clearly constrain source endpoints to mono/stereo.
- config::save truncates the destination directly. Interrupted/concurrent
  writes can destroy a previously valid configuration. Use an appropriate
  atomic replacement strategy, and define concurrent-instance behavior.
- Runtime CLI gain changes are not saved on exit; persist_session runs
  before playback. Clarify this policy or persist the final gain values.
- COM lifetime ordering in App is conditional: _com is declared before
  watcher, and Rust drops fields in declaration order after App::drop.
  If this guard owns the final COM initialization, uninitialization
  precedes callback unregistration/interface release. Eframe's existing
  STA initialization can mask this. Explicitly drop watcher first or
  place the owning COM guard last.
- Worker panics bypass the source stop store / renderer Failed update,
  and join errors are ignored. The health API can then report a live
  engine after its source died. Define panic-to-status handling.
- list_render_devices aborts the whole enumeration when one endpoint's
  name or activation fails. Consider retaining healthy endpoints and
  surfacing a per-device error, especially during hot-plug races.
- capture.rs and PLAN.md incorrectly describe lack of loopback events as
  a general limitation. Microsoft documents direct event-driven loopback
  support since Windows 10 version 1703; this project's minimum is 21H2.
  Polling remains a valid choice, but its rationale should be updated.
- CI push events cover main only. With no implementation PR and no
  existing workflow runs, pushes to the current development branch have
  not established the advertised Windows checks. Open a PR or extend
  the push branch filter, then run the checks on the implementation.

References:
- [Microsoft device events](https://learn.microsoft.com/en-us/windows/win32/coreaudio/device-events)
- [Microsoft loopback recording](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)
- [Rust field drop order](https://doc.rust-lang.org/reference/destructors.html)
- [Microsoft CoUninitialize](https://learn.microsoft.com/en-us/windows/win32/api/combaseapi/nf-combaseapi-couninitialize)

## Release validation still needed

1. Run fmt, clippy --all-targets with denied warnings, build and the
   existing unit tests on Windows for the exact reviewed/fixed commit.
2. Add deterministic tests for controller convergence and gap recovery,
   saved-session CLI overrides, partial startup rollback, and worker
   failure propagation.
3. Validate different endpoint rates and anti-alias suppression.
4. Record simultaneous impulses from two independent clock domains,
   including different device latencies, and report initial offset plus
   drift over at least one hour.
5. Validate target removal/rejoin while another target stays audible,
   removal/rejoin of the last target, rapid replug, format changes,
   source loss, and application shutdown.
6. Smoke-test portable and installed builds on the documented minimum
   Windows version.

The separation of capture, ring, per-device renderers, and control
handles is a useful foundation. The source-equals-target checks,
independent reader cursors, gain ramping, and silent-buffer handling
are sensible. The main release blockers are missing playback-position
alignment and unsafe audio-quality assumptions for downsampling.
