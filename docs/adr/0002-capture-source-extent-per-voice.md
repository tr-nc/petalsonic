# Capture source extent per Voice

Status: accepted

## Context

A distributed ambient source is one playback event whose acoustic power occupies a finite domain.
Representing it as several point Emitters would duplicate playback cursors, make phase and
loudness depend on sample count, and let later Emitter movement change the meaning of an older
Voice. Source shape, Direct Path placement, Environment Send origin, and obstruction policy are
different decisions and must not be folded into one tree- or foliage-specific type.

## Decision

`SourceExtent` is generic Emitter/frame state. `Point` is the compatibility default;
`WeightedSamples` is a bounded, immutable set of stable IDs, local positions, and normalized
power weights. Construction rejects empty or over-capacity sets, duplicate IDs, non-finite or
over-radius positions, and non-positive or non-finite weights.

A complete `SpatialFrame` atomically updates an Emitter's pose and extent. When a play command is
accepted, the current extent and `OcclusionProfile` are copied into the new Voice. Later frames
may move an attached Voice's pose, but never replace the extent or occlusion policy captured by
that Voice. Therefore a later play cannot relocate or reshape an older Voice's immutable acoustic
meaning.

Direct Placement and Environment Origin independently transform the same captured local extent.
The asynchronous worker traces each enabled route's finite samples. Per frequency band it computes

```text
E = sum(weight_i * transmission_i^2)
gain = sqrt(E)
```

instead of converting the first obstruction into a binary whole-source state. Ambient profiles
then apply configurable gain floors, continuous attack/release, and optional Schmitt classification
with minimum dwell. `PointExact` retains existing point behavior.

Rendering keeps one Voice, one cursor, and one decoded PCM block. Stable sample IDs map to at most
four fixed lobe slots. Each lobe receives its energy share, a continuous energy-weighted direction,
and a deterministic fixed all-pass decorrelator. Lobe gains preserve the worker's total band
energy; render state interpolates gains and directions without allocation.

Early reflections connect a bounded set of visible-energy representatives rather than only the
hidden Emitter center. Their selected power is normalized. Late injection remains the shared,
listener-centric solve. This seam can later admit other propagation mechanisms without changing
extent ownership or duplicating Voices.

Completed solves follow the compatibility rules in
[ADR 0003](0003-publish-compatible-acoustic-responses.md). Exact per-sample caches include Voice
route generation, Emitter, spatial revision, geometry version, stable sample ID, and route.
A source skipped by the stable global budget retains its prior response only up to its profile's
age limit, then becomes explicitly deferred; deferred does not mean unity.

## Consequences

- Producers must keep sample IDs stable while representatives describe the same physical region.
- Shape changes are full-frame publications, not low-frequency Emitter property updates.
- Eight samples and four render lobes are hard library bounds; world configuration can impose
  lower solve-wide extent and direct-ray limits.
- The worker may allocate and query geometry. The render path only reads an immutable bounded
  target and uses preallocated state.
- Acoustic telemetry is an independent bounded stream, so lifecycle-event compatibility is not
  coupled to diagnostic volume. Each active route publishes at most eight stable-ID sample
  observations containing normalized power, world position, hit state, and the exact three-band
  material transmission used by aggregation; cache reuse and retained responses preserve those
  observations rather than fabricating misses.
