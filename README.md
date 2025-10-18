# PetalSonic

A real-time safe spatial audio library for Rust that uses Steam Audio for 3D spatialization.

## Overview

PetalSonic makes it easy to add high-quality 3D spatial audio to your Rust applications and games. Whether you're building a game, virtual reality experience, or any application that needs positioned audio, PetalSonic provides a simple, safe, and powerful API.

## Quick Start

Add PetalSonic to your `Cargo.toml`:

```toml
[dependencies]
petalsonic = "0.1"
```

Basic usage example:

```rust
use petalsonic::*;

fn main() -> Result<(), PetalSonicError> {
    // Create the audio world and engine
    let config = PetalSonicWorldDesc::default();
    let world = PetalSonicWorld::new(config.clone())?;
    let mut engine = PetalSonicEngine::new(config, &world)?;
    engine.start()?;

    // Load and play a 3D positioned sound
    let audio = audio_data::PetalSonicAudioData::from_path("sound.wav")?;
    let source_id = world.register_audio(
        audio,
        SourceConfig::spatial(Vec3::new(5.0, 0.0, 0.0), 1.0)
    )?;
    world.play(source_id, playback::LoopMode::Once)?;

    // Update listener position in your game loop
    world.set_listener_pose(Pose::from_position(Vec3::ZERO));

    Ok(())
}
```

## Features

- **High-Quality 3D Spatialization**: Powered by Steam Audio with HRTF-based binaural rendering
- **Real-Time Safe**: Zero allocations and locks in the audio thread
- **Easy to Use**: Simple world-driven API - just load audio, position sources, and play
- **Flexible**: Supports both spatial and non-spatial audio in the same world
- **Event-Driven**: Get notified when sounds complete, loop, or encounter errors
- **Multiple Formats**: Load WAV, MP3, FLAC, OGG, and more via Symphonia
- **Ray Tracing**: Optional ray tracing support for occlusion and reverb effects
- **Cross-Platform**: Works on Windows, macOS, Linux, and more via CPAL

## Project Structure

This project uses a **workspace structure** to separate the core library from demo/example code:

```
petalsonic/
├── Cargo.toml              # Workspace manifest
├── petalsonic/             # Pure audio library
│   ├── Cargo.toml
│   └── src/                # Core library modules
└── petalsonic-demo/        # Demo applications and examples
    ├── Cargo.toml
    └── src/main.rs         # CLI demo and tests
```

### PetalSonic Core Library (`petalsonic`)

**Purpose**: Pure spatial audio processing library with no UI dependencies

**Contains**: Audio engine, world management, spatialization, data loading

**Dependencies**: Only audio-related crates (cpal, audionimbus, symphonia, etc.)

See the [petalsonic README](./petalsonic/README.md) for detailed API documentation.

### Demo Crate (`petalsonic-demo`)

**Purpose**: Examples, tests, and future interactive applications

**Contains**: CLI demos, integration tests, future web UI components

**Run the demo**:
```bash
cargo run --package petalsonic-demo
```

## Basic Codebase Structure

### PetalSonic Core Library (petalsonic)

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
│ - register_audio(audio_data, SourceConfig)                   │
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

## Documentation

- **API Documentation**: Run `cargo doc --open` to generate and view the full API documentation
- **Core Library README**: See [petalsonic/README.md](./petalsonic/README.md) for detailed usage guide
- **Examples**: Check the `petalsonic-demo` crate for working examples

## Development Commands

### Build and Test

```bash
# Build entire workspace
cargo build

# Run demo application
cargo run

# Run tests
cargo test

# Run clippy on workspace
cargo clippy

# Generate documentation
cargo doc --open
```

## License

This project is licensed under the MIT License - see the LICENSE file for details.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## Links

- [Steam Audio](https://valvesoftware.github.io/steam-audio/)
- [Symphonia](https://github.com/pdeljanov/Symphonia)
- [CPAL](https://github.com/RustAudio/cpal)
