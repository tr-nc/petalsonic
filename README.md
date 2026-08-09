# PetalSonic

A real-time safe spatial audio library for Rust that uses Steam Audio for 3D spatialization.

## Overview

PetalSonic makes it easy to add high-quality 3D spatial audio to your Rust applications and games. Whether you're building a game, virtual reality experience, or any application that needs positioned audio, PetalSonic provides a simple, safe, and powerful API.

## Quick Start

Add PetalSonic to your `Cargo.toml`:

```toml
[dependencies]
petalsonic = "0.6"
```

Basic usage example:

```rust
use petalsonic::*;

fn main() -> Result<(), PetalSonicError> {
    // Creating the world starts its private audio runtime.
    let config = PetalSonicWorldDesc::default();
    let world = PetalSonicWorld::new(config)?;

    // Load and play a 3D positioned sound
    let clip = ResidentClip::from_path("sound.wav")?;
    let emitter = world.create_emitter(
        clip,
        EmitterDesc::spatial(Pose::from_position(Vec3::new(5.0, 0.0, 0.0)))
    )?;
    world.play(emitter, PlayOptions::once())?;

    // Publish the listener and all spatial Emitters as one complete game-frame snapshot.
    world.publish_spatial_frame(SpatialFrame::new(
        Pose::from_position(Vec3::ZERO),
        vec![EmitterSpatialState::new(
            emitter,
            Pose::from_position(Vec3::new(5.0, 0.0, 0.0)),
        )],
    ))?;

    Ok(())
}
```

## Features

- **High-Quality 3D Spatialization**: Powered by Steam Audio with HRTF-based binaural rendering
- **Real-Time Safe**: Zero allocations and locks in the audio thread
- **Easy to Use**: Simple world-driven API - just load audio, position sources, and play
- **Flexible**: Supports both spatial and non-spatial audio in the same world
- **Recoverable Output**: Keeps the World alive while devices disappear or the default changes
- **Pull-Based Events**: Observe controlled completion and runtime state on the caller thread
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
│ - create_emitter(ResidentClip, EmitterDesc)                  │
│ - play(emitter, PlayOptions)                                 │
│ - publish_spatial_frame(complete_snapshot)                   │
│ - set_bus_params(), drain_events(), diagnostics()            │
│ - submit bounded intent                                      │
└──────────────────────────────────────────────────────────────┘
                             ↓ bounded control intent
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
- **World-owned render thread**: Creating a world starts audio progress; callers never pump it
- **Emitter/Voice split**: Callers manage stable Emitters; playback Voices are internal
- **World-level listener**: Single global listener pose for all spatial sources
- **Lock-free ring buffer**: Bridges fixed-size render blocks to variable-size device callbacks
- **Real-time safety**: No allocations or locks in the audio callback path
- **Recoverable devices**: Default-device changes preserve the World and rebuild only output state
- **Observable pressure**: Diagnostics expose queue watermarks, rejected work, dropped events,
  underruns, render percentiles, active counts, and device generations

## High-level Goals

- World-driven API on the main thread: you own and update a 3D world (listener + sources).
- Fixed-size audio processing thread(s) that use audionimbus (Steam Audio) for spatialization.
- Decoding with Symphonia; optional resampling on load to a world-wide sample rate.
- Playback via CPAL, with a lock-free SPSC ring buffer bridging fixed-size producer blocks to variable-size device callbacks.
- Real-time safe in the audio callback; no allocations/locks on the RT path.
- One-shot Voices are reclaimed automatically; controlled completion is available when requested.

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

# Run all tests
cargo test --workspace --all-targets

# Run strict clippy on workspace
cargo clippy --workspace --all-targets -- -D warnings

# Generate documentation
cargo doc --open
```

## Publishing to crates.io

The release gate is non-destructive by default and includes formatting, strict static checks,
all workspace tests, documentation tests, a release Demo build, and a registry dry run:

```bash
tools/publish.sh

# Registry publication requires an explicit write flag.
tools/publish.sh --publish
```

**Important notes:**
- Only the `petalsonic` core library is published (not `petalsonic-demo`)
- Ensure all dependencies have compatible versions
- Update CHANGELOG.md and documentation before publishing
- Tag the release only after the publish gate and registry write succeed

## License

This project is licensed under the MIT License - see the LICENSE file for details.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## Links

- [Steam Audio](https://valvesoftware.github.io/steam-audio/)
- [Symphonia](https://github.com/pdeljanov/Symphonia)
- [CPAL](https://github.com/RustAudio/cpal)
