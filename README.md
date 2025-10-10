# PetalSonic

This library is for helping developers to implement spatial audio in their realtime application, easily.

## Basic Codebase Structure

### PetalSonic Core Library (petalsonic-core)

- Pure spatial audio processing
- Steam Audio integration
- Thread-safe audio pipeline
- Audio data loading/resampling
- Real-time safe operations

### Demo Crate (petalsonic-demo)

- Web server and UI framework
- Visual scene representation
- User interaction (drag/drop, controls)
- Scene persistence and presets
- Performance monitoring and debugging tools
- Example integrations and tutorials

## Architecture

### Threading Model

PetalSonic uses a three-layer architecture to provide real-time safe spatial audio:

```plaintext
┌──────────────────────────────────────────────────────────────┐
│ Main Thread (World)                                          │
│ - add_source(audio_data, SourceConfig)                       │
│   * SourceConfig::NonSpatial                                 │
│   * SourceConfig::Spatial { position, volume, ... }          │
│ - set_listener_pose(pose)                                    │
│ - send PlaybackCommand via channel                           │
└──────────────────────────────────────────────────────────────┘
                             ↓ PlaybackCommand + SourceConfig
┌──────────────────────────────────────────────────────────────┐
│ Render Thread (generates samples at world rate)              │
│ - Process PlaybackCommand                                    │
│ - For each active source:                                    │
│   ├─ NonSpatial → direct mixing                              │
│   └─ Spatial → Steam Audio (Direct, Encode, Decode) → mix    │
│ - Push frames to ring buffer                                 │
└──────────────────────────────────────────────────────────────┘
                             ↓ Ring Buffer (StereoFrame)
┌──────────────────────────────────────────────────────────────┐
│ Audio Callback (device rate)                                 │
│ - Consume from ring buffer (lock-free)                       │
│ - Output to device                                           │
└──────────────────────────────────────────────────────────────┘
```

### Key Design Decisions

- **Coexistence**: Spatial and non-spatial sources work together in the same world
- **Render thread does spatial processing**: No separate simulation thread (simpler architecture)
- **Per-source spatial mode**: Each source has `SourceConfig` to determine processing path
- **World-level listener**: Single global listener pose for all spatial sources
- **Lock-free ring buffer**: Bridges fixed-size render blocks to variable-size device callbacks
- **Real-time safety**: No allocations or locks in the audio callback path

## High-level Goals

- World-driven API on the main thread: you own and update a 3D world (listener + sources).
- Fixed-size audio processing thread(s) that use audionimbus (Steam Audio) for spatialization.
- Decoding with Symphonia; optional resampling on load to a world-wide sample rate.
- Playback via CPAL, with a lock-free SPSC ring buffer bridging fixed-size producer blocks to variable-size device callbacks.
- Real-time safe in the audio callback; no allocations/locks on the RT path.
- One-shot and loop sources, with automatic removal of finished one-shots via events.
