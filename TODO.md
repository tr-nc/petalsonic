# PetalSonic Implementation TODO

This is a spatial audio library with real-time safe audio processing using Steam Audio (audionimbus) for spatialization.

You can find reference under src/reference-audio/, you can copy code from there, but you must not change, reference or update anything inside that folder, these are our reference files.

Only install the dependencies where needed.

## Separating core lib code with demo code

### Workspace Structure Plan

```
petalsonic/
├── Cargo.toml          # Workspace manifest
├── README.md
├── TODO.md
├── petalsonic-core/    # The audio library
│   ├── Cargo.toml
│   ├── src/lib.rs
│   └── ...
└── petalsonic-demo/    # Interactive demo application
    ├── Cargo.toml
    ├── src/
    │   ├── main.rs
    │   ├── audio/      # Audio management layer
    │   ├── ui/         # Web UI components
    │   ├── scene/      # 3D scene management
    │   └── input/      # User interaction handling
    └── assets/         # Audio files, UI assets
```

### Demo Crate Architecture (petalsonic-demo)

**Technology Stack:**

- **Web Framework**: `axum` for HTTP server + WebSocket support
- **Frontend**: `leptos` or `yew` for Rust WASM frontend
- **3D Visualization**: `three-d` or `bevy` for 3D scene rendering
- **Audio Bridge**: Custom layer connecting web events to PetalSonic

**Module Structure:**

**`src/audio/`** - Audio Management Layer

- `manager.rs`: Wraps PetalSonicWorld, handles audio thread coordination
- `bridge.rs`: Converts UI events to audio commands
- `presets.rs`: Pre-defined audio scenes and effects

**`src/ui/`** - Web Interface

- `app.rs`: Main web application component
- `components/`: Draggable audio sources, listener controls, scene controls
- `canvas.rs`: 3D scene visualization and interaction
- `websocket.rs`: Real-time communication with audio backend

**`src/scene/`** - Scene Management

- `world.rs`: 3D world state (separate from audio world)
- `objects.rs`: Visual representations of audio sources/listener
- `physics.rs`: Collision detection, movement constraints
- `serialization.rs`: Save/load scene configurations

**`src/input/`** - User Interaction

- `drag.rs`: Drag-and-drop logic for positioning objects
- `gestures.rs`: Multi-touch, zoom, rotate interactions
- `keyboard.rs`: Hotkeys and shortcuts

### Separation of Concerns

**PetalSonic Core Library (petalsonic-core)**

- Pure spatial audio processing
- Steam Audio integration
- Thread-safe audio pipeline
- Audio data loading/resampling
- Real-time safe operations

**Demo Crate (petalsonic-demo)**

- Web server and UI framework
- Visual scene representation
- User interaction (drag/drop, controls)
- Scene persistence and presets
- Performance monitoring and debugging tools
- Example integrations and tutorials

### Development Evolution Path

1. **Phase 1**: Simple CLI examples demonstrating core library features
2. **Phase 2**: Basic web UI with static audio source positioning
3. **Phase 3**: Interactive drag-and-drop with real-time audio updates
4. **Phase 4**: Advanced features (physics, presets, multiplayer, VR integration)

## Adapt different sample rates

- [x] Create a callback function that is required to fill a certain amount of samples, to be consumed directly by the output device (so the samples feeded in must be the final result), this function must not be blocking, and the target sample rate of the filled samples is configured in the world.
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
