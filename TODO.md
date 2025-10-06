# PetalSonic Implementation TODO

## Development Evolution Path

1. **Phase 1**: Simple CLI examples demonstrating core library features
2. **Phase 2**: Basic web UI with static audio source positioning
3. **Phase 3**: Interactive drag-and-drop with real-time audio updates
4. **Phase 4**: Advanced features (physics, presets, multiplayer, VR integration)

## Adapt different sample rates

- [ ] Use rubato to resample the audio to the output sample rate in realtime. The source of the audio is given from the callback function.

## Ring Buffer & Audio Backend (Phase 3)

- [ ] Implement lock-free SPSC ring buffer (`AudioRing`)
- [ ] Set up CPAL integration (`cpal_backend.rs`)
- [ ] Create basic audio output with synthetic tone for testing
- [ ] Ensure real-time safety (no allocations in audio callback)

## Spatialization System (Phase 4)

- [ ] Define `Spatializer` trait interface
- [ ] Create mock spatializer for testing (simple panning)
- [ ] Implement `SourceBlock` struct for spatial processing
- [ ] Set up audionimbus adapter (`audionimbus_adapter.rs`)

## Audio Engine (Phase 5)

- [ ] Create engine command system (`EngineCmd` enum)
- [ ] Implement producer/mixer thread with fixed block processing
- [ ] Add source state management (`SourceState`, `ListenerState`)
- [ ] Implement mixing logic for spatial and non-spatial sources
- [ ] Add one-shot completion detection and event emission
- [ ] Create engine handle for thread-safe communication

## World API (Phase 6)

- [ ] Implement `PetalSonicAudioListener`
- [ ] Create `PetalSonicAudioSource` with builder pattern
- [ ] Build `PetalSonicWorld` main controller
- [ ] Add source management (add/update/remove)
- [ ] Implement listener pose updates
- [ ] Add start/stop functionality
- [ ] Create event polling system

## Testing & Examples (Phase 7)

- [ ] Write unit tests for ring buffer
- [ ] Test audio data loading and resampling
- [ ] Create integration tests with mock spatializer
- [ ] Build example: `play_one_shot.rs`
- [ ] Build example: `loop_spatial.rs`
- [ ] Add performance benchmarks

## Performance Optimization

- [ ] Preallocate scratch buffers for mixing
- [ ] Optimize spatial processing for multiple sources
- [ ] Measure and optimize producer tick time
- [ ] Add metrics collection for underruns/overruns

## Advanced Features (Future)

- [ ] Streaming audio support for long files
- [ ] Device change handling
- [ ] Sample rate adaptation for different devices
- [ ] Additional spatial audio features (reverb, occlusion)

## Key Design Principles

- Real-time safety: No allocations/locks in audio callback
- Main-thread world control with background audio processing
- Fixed-size blocks for predictable performance
- Automatic cleanup of finished one-shot sources
- Spatial audio using Steam Audio for HRTF processing
- Keep it simple and stupid

---

## Spatial Audio Implementation (Multi-Threaded Architecture)

### Architecture Overview

```
┌─────────────────┐         ┌──────────────────┐         ┌─────────────────┐
│   Main Thread   │────────→│ Simulation Thread│────────→│  Audio Thread   │
│                 │         │                  │         │                 │
│ - World state   │         │ - Steam Audio    │         │ - Effect DSP    │
│ - Add/remove    │         │ - Distance calc  │         │ - Ambisonics    │
│ - Positions     │         │ - Air absorption │         │ - Binaural out  │
└─────────────────┘         └──────────────────┘         └─────────────────┘
        │                            │                            │
        │ WorldUpdate               │ SimulationResult           │
        │ (lock-free channel)       │ (triple buffer)           │
        └───────────────────────────┴───────────────────────────┘
```

### Key Design Decisions

- **No ring buffer**: Direct processing in audio callback (Steam Audio is real-time safe)
- **Lock-free audio thread**: Uses triple buffer to read simulation results without blocking
- **Separate simulation thread**: Runs at lower rate (60Hz) vs audio (48kHz)
- **Graceful degradation**: Audio uses last-known results if simulation falls behind
- **Clear ownership**: Each thread owns its data structures

### Data Structures

```rust
// Main → Simulation
pub enum WorldUpdate {
    ListenerMoved { pose: Pose },
    SourceAdded { id: Uuid, position: Vec3, audio_data: Arc<PetalSonicAudioData> },
    SourceMoved { id: Uuid, position: Vec3 },
    SourceRemoved { id: Uuid },
    SourceVolumeChanged { id: Uuid, volume: f32 },
}

// Simulation → Audio (lock-free via triple buffer)
pub struct SimulationResult {
    pub distance_attenuation: f32,
    pub air_absorption: [f32; 3],  // Low, mid, high frequency bands
    pub direction: Vec3,  // Direction relative to listener
}

pub struct SimulationState {
    per_source: HashMap<Uuid, SimulationResult>,
    listener_pose: Pose,
    generation: u64,
}

// Spatial source config (owned by World)
pub struct SpatialSourceConfig {
    pub position: Vec3,
    pub volume: f32,
    pub looping: bool,
}

// Spatial configuration
pub struct SpatialConfig {
    pub sample_rate: u32,
    pub frame_size: usize,
    pub ambisonics_order: u8,     // Default: 2
    pub distance_scaler: f32,     // Default: 10.0
    pub simulation_rate_hz: f32,  // Default: 60.0
    pub hrtf_path: Option<String>, // None = use default
}
```

### Module Structure

```
petalsonic-core/src/
├── spatial/
│   ├── mod.rs           # Public API, types (SpatialSourceConfig, SpatialConfig)
│   ├── simulator.rs     # SpatialSimulator (simulation thread)
│   ├── processor.rs     # SpatialProcessor (audio thread DSP)
│   ├── state.rs         # SimulationState, SimulationResult
│   └── hrtf.rs          # HRTF loading utilities
├── world.rs             # Add spatial source management
├── engine.rs            # Spawn simulation thread, integrate processor
└── playback.rs          # Update for spatial playback
```

### Implementation Plan

#### **Phase 1: Core Infrastructure (Foundation)**

- [ ] Create `spatial/` module structure
- [ ] Define communication types (`WorldUpdate`, `SimulationResult`, `SimulationState`)
- [ ] Add `triple_buffer` crate dependency
- [ ] Implement triple buffer wrapper for lock-free state sharing
- [ ] Add HRTF loading utilities (`spatial/hrtf.rs`)
- [ ] Create `SpatialConfig` with sensible defaults

#### **Phase 2: Simulation Thread**

- [ ] Create `SpatialSimulator` struct with Steam Audio integration
  - [ ] Initialize Context, Simulator, Scene
  - [ ] Set up update_receiver channel
  - [ ] Set up triple buffer writer
- [ ] Implement `run()` loop with update processing
  - [ ] Process WorldUpdate messages (non-blocking)
  - [ ] Handle source add/remove/move
  - [ ] Handle listener updates
- [ ] Add `run_simulation()` with Steam Audio
  - [ ] Set shared inputs (listener pose, ray tracing params)
  - [ ] Set per-source inputs (positions, distance model, air absorption)
  - [ ] Call `simulator.commit()` and `simulator.run_direct()`
  - [ ] Extract simulation outputs (distance attenuation, air absorption)
- [ ] Write results to triple buffer (non-blocking)
- [ ] Add configurable update rate (sleep between iterations)
- [ ] Test simulation thread standalone (logging outputs)

#### **Phase 3: Audio Thread Processing**

- [ ] Create `SpatialProcessor` struct with DSP objects
  - [ ] Initialize Context, HRTF, AmbisonicsDecodeEffect
  - [ ] Set up triple buffer reader
  - [ ] Pre-allocate scratch buffers (input, direct, encoded, summed, decoded)
- [ ] Implement per-source effect management
  - [ ] Create `SpatialSourceEffects` (DirectEffect, AmbisonicsEncodeEffect)
  - [ ] Add `add_source()` to create effect objects
  - [ ] Add `remove_source()` to clean up
- [ ] Implement `process_spatial_sources()` for audio callback
  - [ ] Read latest simulation state (non-blocking try_read)
  - [ ] Clear accumulation buffer
  - [ ] For each spatial source:
    - [ ] Fill input buffer from PlaybackInstance
    - [ ] Apply DirectEffect (distance + air absorption)
    - [ ] Encode to ambisonics with direction
    - [ ] Accumulate to summed buffer
  - [ ] Decode summed ambisonics to binaural stereo
  - [ ] Mix into output buffer
- [ ] Ensure real-time safety (no allocations, no blocking)

#### **Phase 4: World Integration**

- [ ] Add `PetalSonicAudioListener` to `PetalSonicWorld`
  - [ ] Store listener pose
  - [ ] Add `set_listener_pose(&mut self, pose: Pose)`
- [ ] Add spatial source storage
  - [ ] `spatial_sources: HashMap<Uuid, SpatialSourceConfig>`
- [ ] Add communication channel to simulation thread
  - [ ] Create `sim_update_sender: Sender<WorldUpdate>`
  - [ ] Send updates on listener/source changes
- [ ] Implement spatial source management APIs
  - [ ] `add_spatial_source(audio_data, config) -> Result<Uuid>`
  - [ ] `update_spatial_source_position(id, position) -> Result<()>`
  - [ ] `update_spatial_source_volume(id, volume) -> Result<()>`
  - [ ] `remove_spatial_source(id) -> Result<()>`
- [ ] Handle automatic resampling for spatial sources
- [ ] Wire up to existing play/pause/stop commands

#### **Phase 5: Engine Integration**

- [ ] Initialize `SpatialProcessor` on engine start
  - [ ] Create triple buffer (writer for sim, reader for audio)
  - [ ] Pass reader to SpatialProcessor
- [ ] Spawn simulation thread
  - [ ] Create SpatialSimulator with writer and channel receiver
  - [ ] Spawn thread with `run()` loop
  - [ ] Store JoinHandle for cleanup
- [ ] Update audio callback to route spatial playback
  - [ ] Separate spatial vs non-spatial active instances
  - [ ] Call `spatial_processor.process_spatial_sources()`
  - [ ] Mix spatial output with non-spatial output
- [ ] Handle effect object lifecycle
  - [ ] Create effects when PlaybackCommand::Play for spatial source
  - [ ] Remove effects when source finishes or stops
- [ ] Implement graceful shutdown
  - [ ] Send shutdown signal to simulation thread
  - [ ] Join simulation thread on engine drop

#### **Phase 6: Testing & Validation**

- [ ] Unit tests for spatial types (SimulationResult, SpatialConfig)
- [ ] Test simulation thread in isolation
  - [ ] Verify position updates propagate correctly
  - [ ] Check simulation results are computed
- [ ] Test audio processing with mock simulation state
  - [ ] Verify effect chain (Direct → Encode → Decode)
  - [ ] Check buffer management (no overflows)
- [ ] Integration test: Moving listener + static source
- [ ] Integration test: Static listener + moving source
- [ ] Integration test: Multiple sources mixing correctly
- [ ] Test graceful degradation (slow simulation thread)

#### **Phase 7: Optimization & Polish**

- [ ] Profile simulation thread CPU usage
  - [ ] Measure time per simulation run
  - [ ] Tune simulation rate (30Hz? 60Hz? 120Hz?)
- [ ] Profile audio callback latency
  - [ ] Ensure spatial processing fits in buffer time
- [ ] Add metrics/logging
  - [ ] Track simulation lag (generation counter)
  - [ ] Log audio callback overruns
  - [ ] Add optional tracing/profiling hooks
- [ ] Optimize hot paths
  - [ ] Minimize HashMap lookups in audio callback
  - [ ] Consider array-based storage for active sources
- [ ] Test with high source counts (32, 64, 128 sources)
- [ ] Add documentation and examples
  - [ ] Example: `spatial_source_moving.rs`
  - [ ] Example: `listener_moving.rs`
  - [ ] API documentation for spatial module

### Features Included (MVP)

✅ HRTF-based binaural spatialization
✅ Distance attenuation + air absorption
✅ Ambisonics (order 2) for spatial accuracy
✅ Loop and one-shot playback
✅ Per-source volume control
✅ Listener pose updates (position + orientation)
✅ Lock-free audio thread (triple buffer)
✅ Separate simulation thread (better performance)

### Features Deferred (Future)

❌ Occlusion/transmission
❌ Reverb/reflections
❌ Doppler effect
❌ Per-source directivity
❌ Dynamic HRTF switching
❌ Streaming spatial sources

### Performance Targets

- **Simulation rate**: 60Hz (16.6ms per update)
- **Audio callback**: < 10ms processing time (for 1024 samples @ 48kHz = 21ms budget)
- **Max sources**: 64 spatial sources mixing simultaneously
- **Latency**: Triple buffer adds ~1-2 simulation frames (16-33ms) to position updates

### Dependencies to Add

```toml
[dependencies]
triple_buffer = "7.0"
audionimbus = "0.5"  # Already present
crossbeam-channel = "0.5"  # Already present for other use
```

### Notes

- **Triple buffer** prevents audio thread from blocking on simulation state reads
- **Simulation thread** can safely use HashMap, allocations, etc. (not real-time constrained)
- **Audio thread** remains real-time safe (no blocking, no allocations)
- **Generation counter** helps detect if simulation is falling behind
- **Distance scaler** converts game units to Steam Audio units (typically meters)
