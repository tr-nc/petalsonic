# PetalSonic Implementation TODO

This is a spatial audio library with real-time safe audio processing using Steam Audio (audionimbus) for spatialization.

You can find reference under src/reference-audio/, you can copy code from there, but you must not change, reference or update anything inside that folder, these are our reference files.

Only install the dependencies where needed.

## Adapt different sample rates

- [ ] TODO: Create a callback function that is required to fill a certain amount of samples, this function must not be blocking, and the target sample rate of the filled samples is configured in the world.
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
