# PetalSonic

A real-time safe spatial audio library for Rust that uses Steam Audio for 3D spatialization.

[![Crates.io](https://img.shields.io/crates/v/petalsonic.svg)](https://crates.io/crates/petalsonic)
[![Documentation](https://docs.rs/petalsonic/badge.svg)](https://docs.rs/petalsonic)

## Features

- **High-Quality 3D Spatialization**: Steam Audio integration for HRTF-based binaural audio
- **Real-Time Safe**: No allocations or locks in the audio callback path
- **Flexible Source Management**: Support for both spatial and non-spatial audio sources
- **Automatic Resampling**: Audio is automatically resampled to match the world's sample rate
- **Multiple Loop Modes**: Play once, loop infinitely, or loop a specific number of times
- **Event-Driven**: Get notified of playback events (completion, loops, errors)
- **Multiple Audio Formats**: Support for WAV, MP3, FLAC, OGG, and more via Symphonia

## Quick Start

Add this to your `Cargo.toml`:

```toml
[dependencies]
petalsonic = "0.1"
```

### Basic Example

```rust
use petalsonic::*;
use std::sync::Arc;

fn main() -> Result<(), PetalSonicError> {
    // Create a world configuration
    let config = PetalSonicWorldDesc::default();

    // Create the audio world (runs on main thread)
    let world = PetalSonicWorld::new(config.clone())?;

    // Create and start the audio engine (spawns audio thread)
    let mut engine = PetalSonicEngine::new(config, &world)?;
    engine.start()?;

    // Load audio data from file
    let audio_data = audio_data::PetalSonicAudioData::from_path("path/to/audio.wav")?;

    // Register audio with spatial configuration
    let source_id = world.register_audio(
        audio_data,
        SourceConfig::spatial(Vec3::new(5.0, 0.0, 0.0), 1.0) // Position at (5, 0, 0) with volume 1.0
    )?;

    // Play the audio once
    world.play(source_id, playback::LoopMode::Once)?;

    // Update listener position (typically in your game loop)
    world.set_listener_pose(Pose::from_position(Vec3::new(0.0, 0.0, 0.0)));

    // Poll for events
    for event in engine.poll_events() {
        match event {
            PetalSonicEvent::SourceCompleted { source_id } => {
                println!("Audio completed: {:?}", source_id);
            }
            PetalSonicEvent::SourceLooped { source_id, loop_count } => {
                println!("Audio looped: {:?} (iteration {})", source_id, loop_count);
            }
            _ => {}
        }
    }

    Ok(())
}
```

### Non-Spatial Audio Example

```rust
use petalsonic::*;

// Load background music
let music = audio_data::PetalSonicAudioData::from_path("music.mp3")?;

// Register as non-spatial (no 3D effects, just plays normally)
let music_id = world.register_audio(
    music,
    SourceConfig::non_spatial()
)?;

// Play on infinite loop
world.play(music_id, playback::LoopMode::Infinite)?;
```

### Custom Audio Loading

```rust
use petalsonic::audio_data::*;

// Force mono conversion for spatial audio sources
let options = LoadOptions::new()
    .convert_to_mono(ConvertToMono::ForceMono);

let audio = PetalSonicAudioData::from_path_with_options(
    "sound_effect.wav",
    &options
)?;
```

## Architecture

PetalSonic uses a three-layer threading model to ensure real-time safety:

```plaintext
┌──────────────────────────────────────────────────────────────┐
│ Main Thread (World)                                          │
│ - register_audio(audio_data, SourceConfig)                   │
│ - set_listener_pose(pose)                                    │
│ - play(), pause(), stop()                                    │
│ - poll_events()                                              │
└──────────────────────────────────────────────────────────────┘
                             ↓ Commands via channel
┌──────────────────────────────────────────────────────────────┐
│ Render Thread (generates samples at world rate)              │
│ - Process playback commands                                  │
│ - Spatialize audio sources via Steam Audio                   │
│ - Mix sources together                                       │
│ - Push frames to ring buffer                                 │
└──────────────────────────────────────────────────────────────┘
                             ↓ Lock-free ring buffer
┌──────────────────────────────────────────────────────────────┐
│ Audio Callback (device rate)                                 │
│ - Consume from ring buffer (real-time safe)                  │
│ - Output to audio device via CPAL                            │
└──────────────────────────────────────────────────────────────┘
```

### Key Design Principles

- **World-Driven API**: Main thread owns the 3D world state
- **Real-Time Safety**: Audio callback has no allocations, locks, or blocking operations
- **Lock-Free Communication**: Commands sent via channels, audio data via ring buffer
- **Automatic Resampling**: All audio is resampled to world rate on load
- **Mixed Spatialization**: Spatial and non-spatial sources coexist in the same world

## API Overview

### Core Types

- **`PetalSonicWorld`**: Main API for managing audio sources and playback (main thread)
- **`PetalSonicEngine`**: Audio processing engine (dedicated thread)
- **`SourceId`**: Type-safe handle for audio sources
- **`SourceConfig`**: Configuration for spatial vs. non-spatial sources
- **`PetalSonicAudioData`**: Container for loaded and decoded audio data

### Configuration

- **`PetalSonicWorldDesc`**: World configuration (sample rate, channels, buffer size, etc.)
- **`LoadOptions`**: Options for audio loading (mono conversion, etc.)

### Playback Control

- **`LoopMode`**: `Once` or `Infinite`
- **`PlayState`**: `Playing`, `Paused`, or `Stopped`
- **`PlaybackInfo`**: Detailed playback position and timing

### Events

- **`PetalSonicEvent`**: Events emitted by the engine
  - `SourceCompleted`, `SourceLooped`, `SourceStarted`, `SourceStopped`
  - `BufferUnderrun`, `BufferOverrun`
  - `EngineError`, `SpatializationError`

### Math & Spatial

- **`Pose`**: Position + rotation for listener and sources
- **`Vec3`**: 3D vector (from `glam` crate)
- **`Quat`**: Quaternion rotation (from `glam` crate)

## Configuration Options

```rust
use petalsonic::*;

let config = PetalSonicWorldDesc {
    sample_rate: 48000,           // Audio sample rate (Hz)
    block_size: 512,              // Render block size (frames)
    channels: 2,                  // Output channels (stereo)
    buffer_duration: 0.1,         // Ring buffer duration (seconds)
    max_sources: 64,              // Maximum simultaneous sources
    hrtf_path: None,              // Optional custom HRTF data path
};
```

## Performance Considerations

### Real-Time Safety

The audio callback thread is **completely real-time safe**:

- No allocations
- No locks
- No blocking operations
- Only lock-free ring buffer reads

### Buffer Sizing

- **`block_size`**: Smaller = lower latency, higher CPU usage (typical: 256-1024)
- **`buffer_duration`**: Ring buffer size in seconds (typical: 0.1-0.5)
- Balance latency vs. robustness based on your target platform

### Performance Monitoring

```rust
// Get timing information for performance profiling
for event in engine.poll_timing_events() {
    println!("Mixing: {}μs, Spatial: {}μs, Total: {}μs",
        event.mixing_time_us,
        event.spatial_time_us,
        event.total_time_us
    );
}
```

## Advanced Features

### Custom Audio Loaders

Implement `AudioDataLoader` for custom file formats:

```rust
use petalsonic::audio_data::*;

struct MyLoader;

impl AudioDataLoader for MyLoader {
    fn load(&self, path: &str, options: &LoadOptions) -> Result<Arc<PetalSonicAudioData>> {
        // Your custom loading logic
        todo!()
    }
}
```

## Examples

See the `petalsonic-demo` crate for complete examples:

```bash
# Run the demo application
cargo run --package petalsonic-demo
```

## Platform Support

PetalSonic uses:

- **CPAL** for cross-platform audio output (Windows, macOS, Linux, iOS, Android, Web)
- **Symphonia** for audio decoding (supports most common formats)
- **Steam Audio** (audionimbus) for spatialization (auto-installs native library)

## License

This project is licensed under the MIT License - see the LICENSE file for details.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## Links

- [Documentation](https://docs.rs/petalsonic)
- [Steam Audio](https://valvesoftware.github.io/steam-audio/)
