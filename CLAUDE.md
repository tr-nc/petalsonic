# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

PetalSonic is a real-time safe spatial audio library for Rust that uses Steam Audio (audionimbus) for 3D spatialization. It provides a world-driven API where the main thread owns and updates a 3D world (listener + sources), while fixed-size audio processing threads handle spatialization and playback.

## Workspace Architecture

This project uses a **workspace structure** to separate the core library from demo/example code:

```
petalsonic/
├── Cargo.toml              # Workspace manifest
├── petalsonic-core/        # Pure audio library
│   ├── Cargo.toml
│   └── src/                # Core library modules
└── petalsonic-demo/        # Demo applications and examples
    ├── Cargo.toml
    └── src/main.rs         # CLI demo and tests
```

### Core Library (`petalsonic-core`)

**Purpose**: Pure spatial audio processing library with no UI dependencies
**Contains**: Audio engine, world management, spatialization, data loading
**Dependencies**: Only audio-related crates (cpal, audionimbus, symphonia, etc.)

### Demo Crate (`petalsonic-demo`)

**Purpose**: Examples, tests, and future interactive applications
**Contains**: CLI demos, integration tests, future web UI components
**Dependencies**: Core library + UI frameworks when needed

## Common Development Commands

### Build and Test

```bash
# Build entire workspace
cargo build

# Build only core library
cargo build -p petalsonic-core

# Build only demo
cargo build -p petalsonic-demo

# Run demo application
cargo run -p petalsonic-demo

# Run clippy on workspace
cargo clippy

# Run tests (currently in demo crate)
cargo test -p petalsonic-demo
```

### Documentation

```bash
# Generate and open documentation
cargo doc --open

# Generate docs with dependencies
cargo doc --no-deps
```

## Architecture Overview

### Core Components (`petalsonic-core/src/`)

1. **Main Thread API (`world.rs`)**
   - `PetalSonicWorld`: Main controller that owns engine configuration and object registry
   - `PetalSonicAudioSource`: Source instances with position, volume, and playback parameters
   - `PetalSonicAudioListener`: Single listener with pose information
   - Thread-safe command passing to engine via crossbeam channels

2. **Audio Engine (`engine.rs`)**
   - Manages audio processing threads
   - Handles CPAL integration for audio output
   - Coordinates with spatializer for 3D audio processing

3. **Audio Data (`audio_data.rs`)**
   - `PetalSonicAudioData`: Decoded PCM samples stored in Arc for cheap cloning
   - Supports loading from files via Symphonia
   - Built-in resampling and mono conversion capabilities

4. **Configuration (`config.rs`)**
   - `PetalSonicWorldDesc`: Sample rate, block size, channels, buffer settings
   - Builder pattern for configuration
   - Defaults: 48kHz sample rate, 512 block size, stereo output

### Key Design Patterns

- **Real-time safety**: No allocations or locks in audio callback path
- **Lock-free communication**: Crossbeam channels for main thread → engine commands
- **Arc-based data sharing**: Audio data cloned cheaply between threads
- **Fixed block processing**: Consistent processing blocks for predictable timing

### External Dependencies

- **symphonia**: Audio decoding (WAV, MP3, OGG, etc.)
- **cpal**: Cross-platform audio playback
- **audionimbus**: Steam Audio bindings for spatialization
- **rubato**: High-quality audio resampling
- **glam**: Math library for 3D vectors and quaternions
- **crossbeam-channel**: Lock-free communication
- **thiserror**: Structured error handling

### Audio Pipeline

1. **Load Phase**: Decode audio files with Symphonia, optionally resample and convert to mono
2. **World Updates**: Main thread updates listener/sources, sends commands to engine
3. **Processing Thread**: Fixed-block mixing, spatialization via audionimbus, writes to ring buffer
4. **Output Callback**: CPAL callback reads from ring buffer, handles underruns

### Error Handling

All operations return `Result<T, PetalSonicError>` with structured error types:

- Audio device errors
- Format errors
- IO errors
- Loading/resampling errors
- Configuration errors
- Spatialization errors

## Development Guidelines

1. **Performance**: Keep audio callback path allocation-free and lock-free
2. **Testing**: Use mock spatializer for engine tests without audionimbus dependency
3. **Block Sizes**: Target <60% of block time for processing (e.g., <12ms for 1024@48kHz)
4. **Coordinate System**: Right-handed, meters for positions, quaternions for orientation
5. **Thread Safety**: Main thread owns world, engine threads process audio independently
6. **Separation of Concerns**: Core library stays pure, demos/UI go in petalsonic-demo

## Implementation Workflow

### Adding New Core Library Features

1. **Implement in `petalsonic-core/src/`**
   ```bash
   # Work in core library
   cd petalsonic-core/src/
   # Edit relevant modules (world.rs, engine.rs, etc.)
   ```

2. **Update public API in `lib.rs`**
   ```rust
   pub use new_module::NewFeature;
   ```

3. **Test via demo crate**
   ```bash
   # Create demo in petalsonic-demo/src/
   # Add dependency: petalsonic-core = { path = "../petalsonic-core" }
   cargo run -p petalsonic-demo
   ```

### Creating New Demos and Examples

1. **Simple CLI demo**: Add to `petalsonic-demo/src/main.rs`
2. **Complex example**: Create new file in `petalsonic-demo/src/examples/`
3. **Future web UI**: Add web framework dependencies to `petalsonic-demo/Cargo.toml`

Example demo structure:
```rust
use petalsonic_core::*;

fn main() {
    // Demo your feature here
    let config = PetalSonicWorldDesc::default();
    let world = PetalSonicWorld::new(config).unwrap();
    // ...
}
```

### Testing Strategy

1. **Unit tests**: In core library modules (keep minimal)
2. **Integration tests**: In `petalsonic-demo/src/` as executable demos
3. **Real-world testing**: Via interactive demos and examples

### Adding Dependencies

- **Audio/core dependencies**: Add to `petalsonic-core/Cargo.toml`
- **UI/demo dependencies**: Add to `petalsonic-demo/Cargo.toml`
- **Shared dependencies**: Add to workspace `Cargo.toml` and reference via `{ workspace = true }`

## Common Tasks

### Adding a new audio source

1. Load audio data: `PetalSonicAudioData::load_from_path()`
2. Create source with position/volume parameters
3. Add to world: `world.add_source(source)`

### Updating listener position

1. Create new pose with position/orientation
2. Update world: `world.set_listener(listener)` or `world.update_listener(pose)`

### Handling events

1. Poll events each frame: `world.poll_events()`
2. Handle `SourceFinished` events to remove completed one-shot sources

### Audio format considerations

- Spatial sources should be mono for best 3D positioning
- Non-spatial sources can be stereo for music/UI
- All sources resampled to world sample rate on load

### Creating Interactive Demos

1. **Start simple**: CLI demo with keyboard input
2. **Add frameworks**: Web UI (axum + leptos), desktop UI (egui), or game engine (bevy)
3. **Keep separation**: UI logic in demo crate, audio logic in core library
