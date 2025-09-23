# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

PetalSonic is a real-time safe spatial audio library for Rust that uses Steam Audio (audionimbus) for 3D spatialization. It provides a world-driven API where the main thread owns and updates a 3D world (listener + sources), while fixed-size audio processing threads handle spatialization and playback.

## Common Development Commands

### Build and Test

```bash
# Build the project
cargo build

# Run tests
cargo test

# Check if the code is correct and idiomatic
cargo clippy
```

### Documentation

```bash
# Generate and open documentation
cargo doc --open

# Generate docs with dependencies
cargo doc --no-deps
```

## Architecture Overview

### Core Components

1. **Main Thread API (`src/world.rs`)**
   - `PetalSonicWorld`: Main controller that owns engine configuration and object registry
   - `PetalSonicAudioSource`: Source instances with position, volume, and playback parameters
   - `PetalSonicAudioListener`: Single listener with pose information
   - Thread-safe command passing to engine via crossbeam channels

2. **Audio Engine (`src/engine.rs`)**
   - Manages audio processing threads
   - Handles CPAL integration for audio output
   - Coordinates with spatializer for 3D audio processing

3. **Audio Data (`src/audio.rs`)**
   - `PetalSonicAudioData`: Decoded PCM samples stored in Arc for cheap cloning
   - Supports loading from files via Symphonia
   - Built-in resampling and mono conversion capabilities

4. **Configuration (`src/config.rs`)**
   - `PetalSonicConfig`: Sample rate, block size, channels, buffer settings
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
