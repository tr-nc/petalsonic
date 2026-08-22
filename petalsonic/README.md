# PetalSonic

A real-time safe native spatial audio library for Rust.

[![Crates.io](https://img.shields.io/crates/v/petalsonic.svg)](https://crates.io/crates/petalsonic)
[![Documentation](https://docs.rs/petalsonic/badge.svg)](https://docs.rs/petalsonic)

## Features

- **High-Quality 3D Spatialization**: Native HRTF and Ambisonics binaural rendering
- **Real-Time Safe**: No allocations or locks in the audio callback path
- **Flexible Source Management**: Support for both spatial and non-spatial audio sources
- **Automatic Resampling**: Audio is automatically resampled to match the world's sample rate
- **Automatic Recovery**: Keeps logical audio state while output devices change or disappear
- **Pull-Based Events**: Controlled completion and runtime state are observed on the caller thread
- **Runtime Acoustics Control**: Toggle geometry-driven effects without rebuilding spatial audio
- **Multiple Audio Formats**: Support for WAV, MP3, FLAC, OGG, and more via Symphonia

## Quick Start

Add this to your `Cargo.toml`:

```toml
[dependencies]
petalsonic = "0.7"
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

    // Publish one complete listener + Emitter generation per game frame.
    world.publish_spatial_frame(SpatialFrame::new(
        Pose::from_position(Vec3::ZERO),
        vec![EmitterSpatialState::new(
            emitter,
            Pose::from_position(Vec3::new(5.0, 0.0, 0.0)),
        )],
    ))?;

    // This can be changed live. HRTF, distance, and playback remain active.
    world.set_environmental_acoustics_enabled(false)?;

    // Poll for events
    for event in world.drain_events() {
        match event {
            PetalSonicEvent::PlaybackCompleted { emitter, control, tag } => {
                println!("{emitter} completed {control} with tag {tag:?}");
            }
            PetalSonicEvent::RuntimeStateChanged(state) => {
                println!("audio runtime is now {state:?}");
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

### Predecoded PCM

```rust
use petalsonic::ResidentClip;

// A resource system can decode elsewhere and transfer immutable PCM ownership.
let clip = ResidentClip::from_mono_pcm(decoded_samples, 48_000)?;
```

## Architecture

PetalSonic uses a three-layer threading model to ensure real-time safety:

```plaintext
┌──────────────────────────────────────────────────────────────┐
│ Main Thread (World)                                          │
│ - create_emitter(ResidentClip, EmitterDesc)                  │
│ - publish_spatial_frame(complete_snapshot)                   │
│ - play(), pause(), stop()                                    │
│ - drain_events()                                             │
└──────────────────────────────────────────────────────────────┘
                             ↓ Commands via channel
┌──────────────────────────────────────────────────────────────┐
│ Render Thread (generates samples at world rate)              │
│ - Process playback commands                                  │
│ - Apply native acoustics and the fixed world HRTF plan       │
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
- **Bounded Communication**: Non-blocking control/event queues and latest-only spatial snapshots
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

### Playback Control

- **`PlayOptions`**: One-shot or looping intent plus bus, gain, and detachment
- **`PlaybackControl`**: Optional handle for one explicitly controlled Voice

### Events

- **`PetalSonicEvent`**: Events emitted by the engine
  - `PlaybackCompleted` for controlled one-shots
- **`RuntimeStatus` / `RuntimeDiagnostics`**: Current lifecycle state and cumulative bounded-runtime
  health counters

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
    control_queue_capacity: 256,  // Bounded regular control queue
    lifecycle_queue_capacity: 32, // Reserved stop/destroy capacity
    event_queue_capacity: 128,    // Bounded pull-event queue
    timing_queue_capacity: 128,   // Bounded diagnostics queue
    spatial_quality: SpatialQuality::Balanced,
    latency_profile: LatencyProfile::Balanced,
    native_hrtf_path: None,       // Optional custom .petalhrtf; uses the embedded HRTF otherwise
    hrtf_gain: 0.0,               // HRTF gain compensation (dB)
    distance_scaler: 10.0,        // 1 world unit = 10 meters for acoustic queries
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

The release gate compares the warmed 32-Voice, 48 kHz-to-44.1 kHz full-quantum P99 against
`perf/balanced_near_capacity.baseline`. Workload changes require an explicit new baseline, and a
P99 regression beyond the recorded allowance blocks packaging.

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

let health = world.diagnostics();
println!(
    "voices={}, underruns={}, render p99={}μs, device generation={}",
    health.active_voices,
    health.underrun_count,
    health.render_time_p99_us,
    health.device_generation,
);
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
- **Native PetalSonic DSP** for HRTF, Ambisonics, and geometry acoustics

## License

This project is licensed under the MIT License - see the LICENSE file for details.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## Links

- [Documentation](https://docs.rs/petalsonic)
