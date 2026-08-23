# Publish compatible acoustic responses across pose revisions

Status: accepted

## Context

The acoustics worker consumes immutable scene, Voice, and spatial snapshots asynchronously. A
single input generation previously represented every kind of change. At high SpatialFrame rates,
ordinary listener or emitter motion advanced that generation while a solve was running, so the
worker discarded the entire completed solve. A solve taking about 50 ms could therefore be
discarded continuously by 240 Hz pose publication, starving an Environment Send whose PCM Voice
lasted less than one second.

Scene replacement, Voice retirement, captured-route replacement, and pose motion do not have the
same safety meaning. Treating them as one compatibility condition made a scheduling observation
look like an ownership invariant.

## Decision

The worker keeps one wake generation so every input change still schedules work, but publication
compatibility is narrower:

- the captured acoustic scene version must still equal the current scene version;
- each per-Voice result must still match both its Voice identity and worker-assigned route
  generation;
- ordinary listener or emitter pose revision changes do not invalidate completed work.

If the scene changed, the complete result is discarded. If a Voice retired or its captured routing
was replaced, only that Voice's response and telemetry are removed. Compatible results may carry
an older spatial revision, which remains observable. When newer input arrived during the solve,
the worker publishes compatible work and immediately starts solving the latest complete input.

The renderer requires exact current-scene compatibility and monotonic acoustic-response ordering,
but it does not require a response to catch up to the latest render-side pose revision. Per-sample,
temporal-occlusion, retained-response, and budget-membership caches include the Voice route
generation so state cannot leak across a routing lifetime.

## Consequences

- High-frequency pose publication cannot starve a short Environment Send solely by invalidating
  every completed solve.
- Acoustics may intentionally lag the render pose by a bounded worker interval and solve time;
  telemetry reports the captured spatial revision and response age.
- A scene replacement remains a hard compatibility barrier.
- Voice identity reuse or future rerouting is safe even within one unchanged SpatialFrame.
- Rendering keeps one Voice and one PCM cursor; this decision changes only asynchronous response
  compatibility and scheduling.
