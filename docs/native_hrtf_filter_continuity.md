# Native HRTF direction-change continuity

Nearest-direction lookup used to replace the HRIR immediately. Rotation alone
could therefore change adjacent output samples even with constant input and
fixed source/listener positions. The FFT path also retained overlap produced
by the old filter rather than evaluating the new filter against complete history.

Both paths now linearly crossfade complete old/new convolutions over the render
block (512 samples at 48 kHz is about 10.7 ms). This is filter transition, not
listener-pose smoothing or output gain suppression. The first block initializes
directly; empty input leaves the transition state unchanged; reset clears it.
One-sample blocks necessarily finish a transition in one sample.

FFT uses overlap-save with taps-1 input history, shared with the FIR fallback.
Stable direction costs one forward and two inverse transforms. A changing
direction adds two inverse transforms, using already allocated scratch space.
Linear weights sum to one, avoiding an equal-power boost for correlated outputs.
Selection remains nearest-neighbor; this is not continuous spatial interpolation.

Validation on 2026-09-14:

- Red regression: rotation-only constant input jumped 0.19999999 before repair.
  The same test passes for FFT and FIR, checking boundary and within-block steps.
- Complete-history oracle: 23-tap filters, 3/8-sample mixed blocks, repeated
  direction reversals, additive output, empty input and reset all pass.
- Workspace fmt/check/strict clippy; 183 unit, 6 integration and 2 doc tests pass.
  Eight expensive tests remain ignored in the normal suite.
- Ignored release real-table rotation sweep passes: maximum FFT/FIR difference
  0.000000063329935; one-source 512-frame kernel p95 10 microseconds on this host.
  This is not a full-game performance or listening measurement.
- Existing release realtime gate first failed at 108 us vs 107 us while other
  builds were running; the isolated rerun passed. Both logs are retained at
  /tmp/hrtf-realtime.log and /tmp/hrtf-realtime-isolated.log.
- Re: Flora local path override: 979+4 tests and hidden/muted release smoke pass.
  No listening acceptance claimed, and other possible crackle mechanisms
  (clipping, device underrun, environmental path changes) are not ruled out.

Commands:

```sh
cargo test -p petalsonic rotation_only_filter_boundary_is_continuous
cargo test -p petalsonic changing_filters_matches_complete_history_convolutions
cargo test --release -p petalsonic real_table_rotation_sweep_matches_fir -- --ignored --nocapture
tools/publish_realtime_gate.sh
```

Initial validation was local only, without publication. On 2026-09-15 the
Re: Flora user reported that the rotation crackle appeared fixed during
listening and explicitly authorized publishing this repair as patch 0.9.2.
