# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Added generic `SourceExtent::{Point, WeightedSamples}` with stable sample IDs, strict bounded
  validation, normalized source power, and immutable per-Voice capture from complete spatial
  frames.
- Added `OcclusionProfile::AmbientDistributed` with configurable gain floors, attack/release,
  Schmitt thresholds, dwell, retained-response age, and bounded lobe count.
- Added a native one-cursor extended-source solve and renderer: energy-domain three-band
  transmission, stable decorrelated direction lobes, bounded multi-representative early
  reflections, shared late injection, revision-safe caching/publication, and configurable global
  extent/ray budgets.
- Added an independently bounded `AcousticTelemetryEvent` stream with per-route stable sample
  observations (normalized power, world position, hit state, and three-band material
  transmission) plus cumulative ray, cache, extent, lobe, retained, and deferred diagnostics.
- Added immutable per-Voice `DirectPath` and `EnvironmentSend` routing. One PCM cursor now feeds
  independent direct and environmental semantics, including listener-relative direct placement,
  fixed world acoustic origins, direct disablement, and environment disablement.
- Added orthogonal `DirectGeometry` and `DirectPropagation` policies. Local sounds can bypass
  asynchronous transmission while retaining immediate native spatialization.
- Added an independently bounded, opt-in `VoiceTelemetryEvent` stream correlated by
  `PlayCommandId`. It reports the first PCM render block and first matching asynchronous
  environment response, including spatial revisions, placement/origin, geometry version, and
  response age without extending the existing lifecycle-event enum.

### Changed
- Acoustic propagation now tracks active Voices rather than only reusable Emitters, preserving
  independent fixed origins for overlapping playbacks. Early reflections drain per Voice after PCM
  completion and the shared late response continues its bounded decay.
- Existing spatial playback defaults remain world-placed direct audio with simulated transmission
  and an environment send that follows the Emitter.

## [0.7.0] - 2026-08-23

### Added
- Added a latest-value runtime control for geometry-driven environmental acoustics. Disabling it
  bypasses occlusion, reflections, and reverberation while preserving native HRTF, distance,
  air absorption, and playback state.
- Added a shared listener-centric eight-line FDN late-reverb renderer with independent low, mid,
  and high RT60 decay, smoothed parameters, pre-delay, and bounded render-time diagnostics.
- Added a world-owned asynchronous acoustic-propagation worker. It consumes versioned complete
  spatial frames and immutable geometry generations, prioritizes a bounded source set, estimates
  three-band direct transmission and late decay, and exposes solve latency and response age.
- Added bounded first-bounce path sampling with second-segment visibility, frequency-dependent
  material response, fractional delay, and native HRTF or Ambisonics rendering for early
  reflections. Path identities and gains are smoothed across asynchronous response updates.

### Changed
- PetalSonic now uses its native HRTF, Ambisonics, direct-path, early-reflection, and late-reverb
  renderers for every spatial quality profile. A converted NH172 PetalHRTF table is embedded as
  the default.
- Geometry traversal no longer runs on the render thread. Hosts publish an immutable
  `AcousticRayQuerySnapshot`; the render path only swaps a completed, bounded response.
- `SpatialFrame` now carries a monotonic revision and simulation timestamp so propagation never
  combines listener and emitter state from different game generations.

### Removed
- Removed the Steam Audio and AudioNimbus backends, native-library auto-install feature, SOFA
  runtime configuration, and the associated FFI lifecycle.

## [0.6.0] - 2026-08-10

### Added
- Added a world-owned output supervisor with automatic default-device recovery.
- Added generational Emitters, optional controlled playback, fixed buses, atomic spatial frames,
  immutable acoustic scene snapshots, quality profiles, runtime health snapshots, and cumulative
  queue/render/device diagnostics.

### Changed
- Audio rendering now advances automatically for the lifetime of `PetalSonicWorld`.
- The public API is centered on `PetalSonicWorld`, immutable `ResidentClip` resources, and
  opaque value handles; backend plans and render scheduling are internal.
- Stable render paths reuse capacity-bounded voice and mixing storage.
- Stop and destruction commands use reserved bounded capacity and audible voices retire through a
  short de-click ramp. Output sessions fade in after recovery and map logical stereo explicitly to
  the negotiated physical channel layout.
- Static spatial-backend failures are returned during World creation; temporary device absence
  remains recoverable with deterministic retry state.

### Removed
- Removed caller-driven pumping, runtime backend switching, procedural render callbacks, and
  public access to playback, mixer, resampler, and spatial-renderer internals.
- Removed the legacy public audio-data loader/options surface and the unused direct-path override.

## [0.5.0] - 2026-06-07

### Added
- Added procedural playback sources.
- Added native direct path, HRTF, early reflection, and ambisonics spatial rendering backends.
- Added PetalHRTF file support and a SOFA-to-PetalHRTF conversion tool.
- Added runtime spatial backend switching.
- Added output device selection.

### Changed
- Improved native HRTF convolution performance.
- Improved native ambisonics decode performance and quality, including fourth-order native ambisonics and high-frequency preservation.
- Aligned Steam HRTF direction handling with the native spatial paths.

## [0.4.0] - 2026-04-28

### Added
- Added an explicit audio pump API for caller-driven audio buffer refills.
- Added direct path override support and occlusion refresh events.
- Added reflection processing support with bounded realtime settings.

### Changed
- Removed the internal audio render thread in favor of engine-owned pump state.
- Bounded per-frame audio refill work and topped off the audio buffer on each pump.
- Reduced startup underrun warning noise while the audio buffer is warming up.

## [0.1.0] - 2025-01-XX

### Added
- Initial release of PetalSonic
- Real-time safe spatial audio engine using Steam Audio
- World-driven API for managing 3D audio sources
- Support for spatial and non-spatial audio sources in the same world
- Automatic audio resampling to world sample rate
- Multiple loop modes (once, infinite)
- Event-driven architecture for playback notifications
- Audio loading from multiple formats (WAV, MP3, FLAC, OGG) via Symphonia
- Lock-free ring buffer architecture for real-time safety
- Optional ray tracing support for occlusion and reverb
- HRTF-based binaural spatialization
- Performance profiling via timing events
- Custom audio loader support via `AudioDataLoader` trait
- Material system with acoustic presets for ray tracing
- Comprehensive API documentation and examples

### Features
- `PetalSonicWorld` - Main thread API for audio management
- `PetalSonicEngine` - Dedicated audio processing thread
- `PetalSonicAudioData` - Audio data container with reference counting
- `SourceConfig` - Flexible spatial/non-spatial configuration
- `RayTracer` trait - Custom ray tracing implementation support
- Three-layer threading model (main thread, render thread, audio callback)

[Unreleased]: https://github.com/tr-nc/petalsonic/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/tr-nc/petalsonic/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/tr-nc/petalsonic/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/tr-nc/petalsonic/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/tr-nc/petalsonic/compare/v0.1.0...v0.4.0
[0.1.0]: https://github.com/tr-nc/petalsonic/releases/tag/v0.1.0
