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
fn main() -> Result<(), PetalSonicError> {
    // Create a world configuration
    let config = PetalSonicWorldDesc::default();

    // Creating the world starts its private render runtime.
    let world = PetalSonicWorld::new(config)?;

    // Load immutable, predecoded audio.
    let clip = ResidentClip::from_path("path/to/audio.wav")?;

    // Register audio with spatial configuration
    let emitter = world.create_emitter(
        clip,
        EmitterDesc::spatial(Pose::from_position(Vec3::new(5.0, 0.0, 0.0)))
    )?;

    // Play the audio once
    let _playback = world.play_controlled(
        emitter,
        PlayOptions::once(),
        PlaybackTag(7),
    )?;

    // Update listener position (typically in your game loop)
    world.set_listener_pose(Pose::from_position(Vec3::new(0.0, 0.0, 0.0)));

    // Poll for events
    for event in world.drain_events() {
        match event {
            PetalSonicEvent::PlaybackCompleted { emitter, control, tag } => {
                println!("{emitter} completed {control} with tag {tag:?}");
            }
        }
    }

    Ok(())
}
```

### Non-Spatial Audio Example

```rust
use petalsonic::*;

// Load background music
let music = ResidentClip::from_path("music.mp3")?;

// Register as non-spatial (no 3D effects, just plays normally)
let music_emitter = world.create_emitter(
    music,
    EmitterDesc::non_spatial()
)?;

// Play on infinite loop
world.play(music_emitter, PlayOptions::looping())?;
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
│ - create_emitter(ResidentClip, EmitterDesc)                  │
│ - set_listener_pose(pose)                                    │
│ - play(), pause(), stop()                                    │
│ - drain_events()                                             │
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
- **`Emitter`**: Generational handle for a logical sound emitter
- **`EmitterDesc`**: Low-frequency spatial/non-spatial defaults
- **`ResidentClip`**: Immutable, predecoded PCM shared by Voices

### Configuration

- **`PetalSonicWorldDesc`**: World configuration and executable capacity limits
- **`LoadOptions`**: Options for audio loading (mono conversion, etc.)

### Playback Control

- **`LoopMode`**: `Once` or `Infinite`
- **`PlaybackControl`**: Optional handle for one explicitly controlled Voice

### Events

- **`PetalSonicEvent`**: Events emitted by the engine
  - `PlaybackCompleted` for controlled one-shots

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
    max_emitters: 64,             // Maximum long-lived emitters
    max_voices: 128,              // Maximum simultaneous playback voices
    control_queue_capacity: 256,  // Bounded non-blocking control queue
    event_queue_capacity: 128,    // Bounded pull-event queue
    timing_queue_capacity: 128,   // Bounded diagnostics queue
    spatial_quality: SpatialQuality::Balanced,
    latency_profile: LatencyProfile::Balanced,
    steam_hrtf_path: None,        // Optional Steam Audio SOFA path
    native_hrtf_path: None,       // Optional native .petalhrtf path
    hrtf_gain: 0.0,               // HRTF gain compensation (dB)
    distance_scaler: 10.0,        // 1 world unit = 10 meters in spatial simulation
    ..Default::default()
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
- Balance latency vs. robustness based on your target platform

### Performance Monitoring

```rust
// Get timing information for performance profiling
for event in world.drain_timing_events() {
    println!(
        "Mixing: {}μs (direct {}μs, spatial {}μs), physics {}μs, encode {}μs, decode {}μs, total {}μs",
        event.mixing_time_us,
        event.direct_mixing_time_us,
        event.spatial_time_us,
        event.spatial_simulation_time_us,
        event.ambisonics_encoding_time_us,
        event.ambisonics_decoding_time_us,
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
