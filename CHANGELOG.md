# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2025-01-XX

### Added
- Initial release of PetalSonic Core
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

[Unreleased]: https://github.com/yourusername/petalsonic/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/yourusername/petalsonic/releases/tag/v0.1.0
