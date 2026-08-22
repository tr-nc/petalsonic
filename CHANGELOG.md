# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- PetalSonic now uses its native HRTF, Ambisonics, direct-path, and early-reflection renderers for
  every spatial quality profile. A converted NH172 PetalHRTF table is embedded as the default.

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

[Unreleased]: https://github.com/tr-nc/petalsonic/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/tr-nc/petalsonic/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/tr-nc/petalsonic/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/tr-nc/petalsonic/compare/v0.1.0...v0.4.0
[0.1.0]: https://github.com/tr-nc/petalsonic/releases/tag/v0.1.0
