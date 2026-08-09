//! # PetalSonic Core
//!
//! A real-time safe spatial audio library for Rust that uses Steam Audio for 3D spatialization.
//!
//! PetalSonic provides a world-driven API where the main thread owns and updates a 3D world
//! (listener + sources), while fixed-size audio processing threads handle spatialization and
//! playback in a real-time safe manner.
//!
//! ## Quick Start
//!
//! ```no_run
//! use petalsonic::*;
//! use petalsonic::math::{Pose, Vec3};
//!
//! // Create a world configuration
//! let config = PetalSonicWorldDesc::default();
//!
//! // Creating the world also starts its private render runtime.
//! let world = PetalSonicWorld::new(config)?;
//!
//! // Load immutable, resident PCM and bind it to an emitter.
//! let clip = ResidentClip::from_path("audio.wav")?;
//!
//! let emitter = world.create_emitter(
//!     clip,
//!     EmitterDesc::spatial(Pose::from_position(Vec3::new(5.0, 0.0, 0.0)))
//! )?;
//!
//! // Simple playback creates and reclaims its Voice internally.
//! world.play(emitter, PlayOptions::once())?;
//!
//! // Update listener position as your camera/player moves
//! world.set_listener_pose(Pose::from_position(Vec3::new(0.0, 0.0, 0.0)));
//!
//! // Pull events on the caller thread when controlled playback is used.
//! let _events = world.drain_events();
//! # Ok::<(), PetalSonicError>(())
//! ```
//!
//! ## Key Components
//!
//! - **[`PetalSonicWorld`]**: Main API for managing audio sources and playback on the main thread
//! - **[`Emitter`]**: Opaque, generational handle for a logical sound emitter
//! - **[`ResidentClip`]**: Immutable, predecoded PCM shared by playback voices
//! - **[`PetalSonicEvent`]**: Events emitted by the engine (completion, errors, etc.)
//!
//! ## Architecture
//!
//! PetalSonic uses a three-layer threading model owned entirely by the world:
//!
//! 1. **Main Thread**: Owns `PetalSonicWorld`, loads audio, and submits intent
//! 2. **World-owned Render Thread**: Processes commands, spatializes audio, generates samples
//! 3. **Audio Callback**: Lock-free consumption from ring buffer to audio device
//!
//! This architecture ensures real-time safety: no allocations or locks in the audio callback path.
//!
//! ## Features
//!
//! - Steam Audio integration for high-quality HRTF-based spatialization
//! - Support for both spatial and non-spatial audio sources
//! - Real-time safe audio processing
//! - Automatic resampling to world sample rate
//! - Loop modes: once, infinite, or counted loops
//! - Event-driven architecture for playback notifications
//! - Performance profiling via timing events

pub mod acoustics;
pub mod audio_data;
pub mod config;
mod domain;
mod engine;
pub mod error;
pub mod events;
pub mod gain;
pub mod math;
mod mixer;
mod playback;
pub mod spatial;
mod world;

pub use acoustics::{
    AcousticHit, AcousticMaterial, AcousticRay, BatchedAnyHitRayTracer, BatchedClosestHitRayTracer,
};
pub use config::PetalSonicWorldDesc;
pub use domain::{Emitter, EmitterDesc, PlayOptions, PlaybackControl, PlaybackTag, ResidentClip};
pub use engine::AudioOutputDeviceInfo;
pub use error::PetalSonicError;
pub use events::{PetalSonicEvent, RenderTimingEvent};
pub use gain::{db_to_linear, linear_to_db};
pub use playback::LoopMode;
pub use spatial::{
    DEFAULT_NATIVE_AMBISONICS_ORDER, DirectOcclusionDebugSnapshot, DirectPathOverride,
    DirectPathTransmission, NativeAmbisonicsBinauralDecoder, NativeAmbisonicsBinauralState,
    NativeAmbisonicsEncoder, NativeHrtfDirection, NativeHrtfRenderMetrics, NativeHrtfRenderer,
    NativeHrtfSourceState, NativeHrtfTable, native_ambisonics_channel_count,
};
pub use world::PetalSonicWorld;
