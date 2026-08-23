# Extended Source Contract

PetalSonic represents a distributed ambient event as one Voice whose power has a finite local
domain. Consumers do not create one Emitter or decoder per representative.

## Producer contract

Create a validated extent once from stable local representatives:

```rust
use petalsonic::{
    DistributedOcclusionProfile, EmitterDesc, ExtentSample, ExtentSampleId, OcclusionProfile,
    Pose, SourceExtent, Vec3,
};

let extent = SourceExtent::weighted_samples(vec![
    ExtentSample::new(ExtentSampleId(10), Vec3::new(-1.0, 0.0, 0.0), 2.0)?,
    ExtentSample::new(ExtentSampleId(20), Vec3::new( 1.0, 0.0, 0.0), 1.0)?,
])?;
let profile = OcclusionProfile::AmbientDistributed(
    DistributedOcclusionProfile::default()
        .with_lobe_count(3)?
        .with_response_times(0.20, 0.15)?
        .with_classification(0.25, 0.55, 0.12)?,
);
let desc = EmitterDesc::spatial(Pose::identity())
    .with_extent(extent.clone())
    .with_occlusion_profile(profile);
# Ok::<(), petalsonic::PetalSonicError>(())
```

IDs identify representatives, not vector indices. Preserve an ID while that representative keeps
the same physical meaning. Input order is irrelevant: construction sorts by ID and normalizes
relative power. The current hard bounds are `MAX_EXTENT_SAMPLES == 8`,
`MAX_DIRECT_LOBES == 4`, and `MAX_EXTENT_RADIUS_WORLD_UNITS` for local positions.

`Point` remains the default for existing consumers. Non-spatial Emitters reject extent and
occlusion semantics.

## Ownership and updates

The Emitter owns the current extent used by future plays. Publish dynamic shape and pose together
in every complete frame:

```rust
world.publish_spatial_frame(SpatialFrame::new(
    revision,
    sim_time_seconds,
    listener_pose,
    vec![EmitterSpatialState::new(emitter, emitter_pose).with_extent(extent)],
))?;
# Ok::<(), petalsonic::PetalSonicError>(())
```

Frame revision must increase and simulation time must not move backward. Listener and Emitter
poses, transformed sample positions, priorities, and extent contents must be finite. A complete
frame must name every live spatial Emitter exactly once.

Accepted play captures `SourceExtent` and `OcclusionProfile` into an immutable Voice. An attached
Voice follows later Emitter poses, but its local extent does not change. A detached Voice follows
neither. `update_emitter` may change low-frequency properties and the profile for future Voices;
it rejects extent changes because those belong to complete `SpatialFrame` publication.

## Orthogonal concepts

- `SourceExtent` says where source power exists in local space.
- `DirectPlacement` says how that local extent reaches the audible Direct Path.
- `EnvironmentOrigin` says how the same local extent reaches environmental solving.
- `OcclusionProfile` says how sampled transmission becomes a stable response.

World Direct Placement, listener-relative Direct Placement, following Environment Origin, and
fixed world Environment Origin transform the extent independently. Disabling either route does
not change the other route or the extent.

## Acoustic and render invariants

For normalized powers, each frequency band uses
`sqrt(sum(weight * transmission^2))`. One sample changing obstruction changes only its energy
term. For example, one of eight equal samples changing from unity to 0.1 produces approximately
`-0.574 dB`, not a whole-source 20 dB step.

The worker returns at most the configured number of stable directional lobes. Sample-to-lobe
membership is `stable_id mod lobe_count`; direction is an energy-weighted centroid with stable-ID
fallbacks. Per-band squared lobe gains sum to the aggregate squared gain, and lobe power sums to
one. Fixed per-slot all-pass decorrelation prevents simple in-phase signal replication.

The render thread fills one mono block and advances one cursor once. It applies preallocated lobe
filters, direction interpolation, decorrelation, and one shared Ambisonics decode. It performs no
ray queries, classification, locks, or per-block allocation.

## Budget, cache, and publication

`PetalSonicWorldDesc::environmental_acoustics_budget` caps processed extents and direct/environment
rays after the quality plan. Ranking is deterministic and favors priority, current audibility,
proximity, and previous membership to reduce churn. A selected route costs one ray per extent
sample. Profile `lobe_count` is independently bounded.

The sample cache key contains Voice and Emitter identity, spatial revision, geometry version,
stable sample ID, and route. A skipped Voice yields `Retained` while its previous response is
within `max_response_age_seconds`; after that it yields `Deferred`. Rendering holds its existing
target for a deferred response instead of jumping to unity.

A completed worker solve is discarded when any newer input generation exists. Rendering rejects
spatial or geometry revision rollback even if a stale response reaches its boundary.

## Environment Response

Early reflection candidates choose at most two representatives ranked by visible transmitted
energy. Their selected power is renormalized, path IDs incorporate the stable sample ID, and the
existing reflection-ray budget is not multiplied by representative count. Late reverberation
continues to use the shared listener-centric solve and receives the extent's aggregated Environment
Send energy. Voice completion drains bounded early state and the shared late tail without keeping
the PCM cursor alive.

## Telemetry

Drain `PetalSonicWorld::drain_acoustic_telemetry` independently from lifecycle and opt-in Voice
render telemetry. `AcousticExtentTelemetry` reports publication and response revisions, Voice and
Emitter identity, sample count, per-route rays/cache hits/hits/visible fraction/raw and filtered
three-band gains/classification/dwell, lobe directions and energy, solve status, response age, and
budget membership. `SolveDiscarded` reports superseded solves.

Use `acoustic_telemetry_diagnostics` for queue depth, high-water mark, and dropped events. Runtime
diagnostics also expose cumulative direct rays, cache hits, processed extents, lobes, retained
responses, deferred responses, and render-boundary revision rejections.
