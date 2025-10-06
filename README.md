# PetalSonic

This library is for helping developers to implement spatial audio in their realtime application, easily.

## Basic Codebase Structure

### PetalSonic Core Library (petalsonic-core)

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

High-level goals

- World-driven API on the main thread: you own and update a 3D world (listener + sources).
- Fixed-size audio processing thread(s) that use audionimbus (Steam Audio) for spatialization.
- Decoding with Symphonia; optional resampling on load to a world-wide sample rate.
- Playback via CPAL, with a lock-free SPSC ring buffer bridging fixed-size producer blocks to variable-size device callbacks.
- Real-time safe in the audio callback; no allocations/locks on the RT path.
- One-shot and loop sources, with automatic removal of finished one-shots via events.
