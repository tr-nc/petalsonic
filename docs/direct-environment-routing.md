# Direct Path and Environment Routing

PetalSonic can give one playback Voice two independent spatial meanings without creating a second
Voice or cursor:

```text
ResidentClip -> one Voice cursor -> one mono PCM block
                                  |-> DirectPath -> audible direct contribution
                                  `-> EnvironmentSend -> early reflections + shared late response
```

The render thread advances and decodes the Voice once per block. Both routes consume that same
sample-aligned block. `EnvironmentSend` itself is not mixed as a second direct contribution.

## Compatibility defaults

`PlayOptions::once()` and `PlayOptions::looping()` preserve the existing spatial behavior unless a
route is overridden:

| Concern | Default |
| --- | --- |
| Direct placement | `DirectPlacement::World` |
| Direct geometry | `DirectGeometry::SimulatedTransmission` |
| Direct propagation | `DirectPropagation::Immediate` |
| Environment origin | `EnvironmentOrigin::FollowEmitter` |
| Environment send gain | `0 dB` |

Non-spatial Emitters reject spatial route or spatial telemetry overrides.

## Body-relative direct with a world-space acoustic origin

Local player footsteps can follow listener translation without inheriting camera pitch while the
environment responds at the physical contact point:

```rust
use petalsonic::{
    DirectGeometry, DirectPath, DirectPropagation, EnvironmentSend, PlayCommandId, PlayOptions,
    Pose,
};

let options = PlayOptions::once()
    .with_direct_path(
        DirectPath::listener_position_relative(feet_world_offset_pose)
            .with_geometry(DirectGeometry::BypassTransmission)
            .with_propagation(DirectPropagation::Immediate),
    )
    .with_environment_send(
        EnvironmentSend::from_world_pose(contact_world_pose)
            .with_gain_db(environment_send_db),
    )
    .with_play_command_id(PlayCommandId(footstep_sequence));

world.play(emitter, options)?;
# Ok::<(), petalsonic::PetalSonicError>(())
```

`ListenerPositionRelative` treats the captured pose as a world-axis offset from the current
listener position. Translation therefore cannot leave the direct sound behind, while every render
block still projects the gravity-aligned offset through the latest listener orientation. Looking
down moves a world-down footstep toward listener-local front. `ListenerRelative` remains available
when both translation and rotation must preserve an invariant listener-local x=right, y=up,
z=front pose.

`EnvironmentSend::from_world_pose` is copied into the new Voice. Reusing or moving the Emitter for
the next footstep does not relocate an older Voice's acoustic response. Use
`EnvironmentSend::follow_emitter` for ordinary moving world sources and
`EnvironmentSend::disabled` when no environmental contribution is wanted.

## Orthogonal policies

`DirectPlacement` decides where direct audio is rendered. `DirectGeometry` independently decides
whether the latest asynchronous transmission response affects it. `DirectPropagation` states the
timing model separately; `Immediate` is the currently supported policy. Bypassing transmission
does not bypass HRTF, distance attenuation, or air absorption.

Disabling a Direct Path requires `BypassTransmission`. A disabled Environment Send uses `0 dB`.
All supplied poses and gains must be finite.

`SourceExtent` is a fourth independent concern: it describes local source power, not route
placement or obstruction policy. Both routes transform the Voice-captured extent independently.
See the [Extended Source contract](extended-source-routing.md) for ownership, stable sample,
energy, lobe, budget, and telemetry rules.

Worker acoustic telemetry preserves this route separation: `AcousticExtentTelemetry::direct` and
`::environment` each carry their own bounded stable-ID sample observations, world positions, hit
states, and three-band transmission snapshots. A cache hit reuses the observation for that exact
route; it is not reported as a miss on either route.

## Response and tail lifecycle

Early reflections are owned by bounded per-Voice state. When direct PCM completes, active early
reflection state enters a draining phase and is released only after its smoothed taps fall below
the bounded release threshold. Late reverberation is a shared listener-centric response; it keeps
rendering its bounded FDN decay after the originating Voice completes. Neither tail keeps the PCM
cursor alive or decodes the clip again.

## Correlated telemetry

Telemetry is opt-in through `PlayOptions::with_play_command_id`. The caller owns ID uniqueness.
PetalSonic exposes these through `PetalSonicWorld::drain_voice_telemetry`, independently of
existing lifecycle events:

- `VoiceTelemetryEvent::FirstRendered` when the spatial Voice first advances its PCM cursor in an
  active render pass. It reports the render-block index, current complete `SpatialFrame` revision,
  listener-local direct pose, captured world acoustic origin, and any response already available
  for that Voice.
- `VoiceTelemetryEvent::EnvironmentResponse` once if the first matching asynchronous response
  arrives later. It reports the response's spatial revision, geometry version, and age when
  observed by rendering.
- `VoiceTelemetryEvent::EnergySummary` once after Voice completion and its bounded early tail have
  drained. It reports cumulative source, direct, Environment Send, and early processing-stage
  energy, plus a snapshot of global late pre-delay, RT60, wet gain, and cumulative input/output
  energy.

If a matching response exists on the first render block, it is embedded in `FirstRendered` and no
second response event is emitted. Telemetry uses a separate queue bounded by
`event_queue_capacity`; consumers should drain it regularly and monitor
`PetalSonicWorld::voice_telemetry_diagnostics`. Keeping the stream separate preserves exhaustive
matches over the pre-existing `PetalSonicEvent` lifecycle API.
