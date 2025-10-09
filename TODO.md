# PetalSonic Implementation TODO

## Advanced Features (Planned for Future)

### Core

- [ ] Device switch handling
- [ ] Additional spatial audio features (reverb, occlusion)

### Demo

- [ ] Basic web UI with static audio source positioning
- [ ] Interactive drag-and-drop with real-time audio updates

---

## Spatial Audio Implementation

### Architecture Overview

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
│   ├─ NonSpatial → current mixing logic                       │
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

- **Coexistence**: Spatial and non-spatial sources work together
- **Render thread does spatial processing**: No separate simulation thread (simpler architecture)
- **Per-source spatial mode**: Each source has `SourceConfig` to determine processing path
- **World-level listener**: Single global listener pose for all spatial sources
- **Existing threading model**: Use current render thread + ring buffer + audio callback
- **Learn from reference-audio**: Study `petalsonic-core/src/reference-audio/spatial_sound_manager.rs` for Steam Audio API patterns

### Reference Code for Steam Audio Usage

**IMPORTANT**: The `petalsonic-core/src/reference-audio/` directory contains working examples of Steam Audio integration. Study these files:

- **`spatial_sound_manager.rs`**: Complete Steam Audio setup (Context, Scene, Simulator, HRTF, effect chains)
  - See how to initialize Steam Audio objects
  - See Direct/Ambisonics effect chains
  - See per-source effect management
- **`spatial_sound.rs`**: Per-source spatial configuration
- **`pal.rs`**: Steam Audio Context/Scene/Simulator initialization patterns

**Note**: Ignore Kira-specific code. Extract only the Steam Audio (audionimbus) API patterns.

### Data Structures

```rust
// Source configuration (per-source spatial mode)
pub enum SourceConfig {
    NonSpatial,
    Spatial {
        position: Vec3,
        volume: f32,
        // Additional spatial params (distance model, air absorption, etc.)
    },
}

// World-level listener
pub struct PetalSonicAudioListener {
    pose: Pose,  // Position + orientation
}

// Spatial configuration for engine
pub struct SpatialConfig {
    pub ambisonics_order: u8,     // Default: 2
    pub distance_scaler: f32,     // Default: 10.0 (converts game units to meters)
    pub hrtf_path: Option<String>, // None = use default HRTF
}
```

### Module Structure

```
petalsonic-core/src/
├── spatial/
│   ├── mod.rs           # Public API, types (SpatialConfig)
│   ├── effects.rs       # Steam Audio effect management (Direct, Encode, Decode)
│   ├── processor.rs     # Spatial processing logic for render thread
│   └── hrtf.rs          # HRTF loading utilities
├── config/
│   ├── mod.rs           # Re-exports
│   └── source_config.rs # SourceConfig enum
├── mixer.rs             # Extract mixing logic from engine
├── world.rs             # Add listener management + SourceConfig API
├── engine.rs            # Initialize spatial processor, route to mixer
└── playback.rs          # Store SourceConfig in PlaybackInstance
```

### Implementation Plan

#### **Stage 1: Refactor for Coexistence (Foundation)**

**Goal**: Separate spatial/non-spatial modes, refactor mixing into clean modules, prepare for Steam Audio integration.

- [ ] Create module structure
  - [ ] Create `config/source_config.rs` with `SourceConfig` enum
  - [ ] Create `mixer.rs` module (extract mixing logic from engine)
  - [ ] Create `spatial/mod.rs` (placeholder for Stage 2)
- [ ] Define `SourceConfig` enum
  - [ ] `NonSpatial` variant
  - [ ] `Spatial { position, volume }` variant
- [ ] Update `PetalSonicWorld` API
  - [ ] Change `add_source()` to accept `(audio_data, SourceConfig)`
  - [ ] Add listener management: `set_listener_pose(&mut self, pose)`
  - [ ] Store listener pose in `PetalSonicAudioListener`
- [ ] Update `PlaybackCommand` and `PlaybackInstance`
  - [ ] Store `SourceConfig` in `PlaybackInstance`
  - [ ] Pass config through `PlaybackCommand::Play`
- [ ] Refactor render thread mixing
  - [ ] Extract `mix_playback_instances()` to `mixer.rs`
  - [ ] Add branching: if spatial → silence (placeholder), else → current logic
  - [ ] Clean up `engine.rs` by using mixer module
- [ ] Update demo/tests to use new API
  - [ ] All existing sources use `SourceConfig::NonSpatial`

**Validation**: All existing functionality works, spatial sources output silence.

---

#### **Stage 2: Steam Audio Integration (Implementation)**

**Goal**: Implement full spatial audio processing in render thread, learn from reference-audio examples.

- [ ] Study reference-audio implementation
  - [ ] Read `spatial_sound_manager.rs` (lines 1-500) for initialization patterns
  - [ ] Identify Steam Audio object lifecycle (Context, Scene, Simulator, HRTF)
  - [ ] Understand effect chain: Direct → AmbisonicsEncode → (accumulate) → AmbisonicsDecode
- [ ] Initialize Steam Audio in engine
  - [ ] Create `SpatialProcessor` struct in `spatial/processor.rs`
  - [ ] Initialize Context, Scene, Simulator (see `pal.rs` for patterns)
  - [ ] Load HRTF (see `spatial_sound_manager.rs` for HRTF loading)
  - [ ] Initialize AmbisonicsDecode effect (shared across sources)
- [ ] Implement per-source effect management
  - [ ] Create `SpatialSourceEffects` struct in `spatial/effects.rs`
  - [ ] Store DirectEffect + AmbisonicsEncodeEffect per source
  - [ ] Add `create_effects_for_source(source_id, config)` method
  - [ ] Add `remove_effects_for_source(source_id)` method
- [ ] Implement spatial processing in render thread
  - [ ] Update `mixer.rs` to call spatial processor for spatial sources
  - [ ] In `SpatialProcessor`, implement processing loop:
    - [ ] Get source audio samples from PlaybackInstance
    - [ ] Apply DirectEffect (distance attenuation, air absorption)
    - [ ] Apply AmbisonicsEncodeEffect (encode with direction)
    - [ ] Accumulate to ambisonics buffer
  - [ ] After all sources processed:
    - [ ] Apply AmbisonicsDecode (convert to binaural stereo)
    - [ ] Mix spatial output with non-spatial output
- [ ] Wire listener pose updates
  - [ ] Pass listener pose from World → Engine → SpatialProcessor
  - [ ] Update Steam Audio simulator with listener pose each frame
- [ ] Handle effect lifecycle
  - [ ] Create effects when PlaybackCommand::Play for spatial source
  - [ ] Remove effects when source stops/finishes
- [ ] Add error handling and logging
  - [ ] Log Steam Audio initialization
  - [ ] Handle Steam Audio errors gracefully

**Validation**: Spatial sources produce spatialized audio, coexist with non-spatial sources.

---

#### **Stage 3: Polish & Optimization**

**Goal**: Performance tuning, testing, documentation.

- [ ] Performance profiling
  - [ ] Measure render thread CPU usage with spatial sources
  - [ ] Ensure spatial processing fits in render thread budget
  - [ ] Test with multiple spatial sources (8, 16, 32+)
- [ ] Testing
  - [ ] Unit tests for SourceConfig
  - [ ] Integration test: static listener + moving spatial source
  - [ ] Integration test: moving listener + static spatial source
  - [ ] Integration test: mix of spatial + non-spatial sources
- [ ] API refinement
  - [ ] Add `update_source_position(source_id, position)` to World
  - [ ] Add `update_source_volume(source_id, volume)` to World
- [ ] Documentation
  - [ ] Document SourceConfig API
  - [ ] Add example: `spatial_audio_demo.rs`
  - [ ] Document Steam Audio configuration options
- [ ] Optimization
  - [ ] Pre-allocate buffers for spatial processing
  - [ ] Minimize allocations in render thread
  - [ ] Consider batch processing for multiple spatial sources

**Validation**: Production-ready spatial audio with good performance.

---

### Features Included (MVP)

✅ Coexistence of spatial and non-spatial sources
✅ HRTF-based binaural spatialization
✅ Distance attenuation + air absorption
✅ Ambisonics encoding/decoding
✅ World-level listener pose
✅ Per-source spatial configuration
✅ Clean module separation

### Features Deferred (Future)

❌ Occlusion/transmission
❌ Reverb/reflections
❌ Doppler effect
❌ Per-source directivity
❌ Dynamic HRTF switching
❌ Source streaming (currently all sources are pre-loaded)

### Dependencies (Already Present)

```toml
[dependencies]
audionimbus = { version = "0.8.1", features = ["auto-install"] }  # Steam Audio
crossbeam-channel = "0.5.13"  # Command channels
ringbuf = "0.4.7"  # Ring buffer
glam = { workspace = true }  # Vec3, math
```

### Notes

- **No separate simulation thread**: Spatial processing happens in existing render thread (simpler)
- **Render thread is not real-time critical**: Can do Steam Audio processing safely
- **Audio callback remains lock-free**: Only consumes from ring buffer
- **Learn from reference-audio**: Don't reinvent, adapt existing patterns
- **Staged approach**: Stage 1 = foundation (non-breaking), Stage 2 = spatial implementation
