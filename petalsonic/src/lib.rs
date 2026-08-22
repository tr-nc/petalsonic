//! # PetalSonic Core
//!
//! A real-time safe native spatial audio library for Rust.
//!
//! PetalSonic provides a world-driven API where the main thread owns and updates a 3D world
//! (listener + sources), while fixed-size audio processing threads handle spatialization and
//! playback in a real-time safe manner.
//!
//! ## Quick Start
//!
//! ```no_run
//! use petalsonic::*;
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
//! // Publish listener and every spatial emitter as one complete generation.
//! world.publish_spatial_frame(SpatialFrame::new(
//!     Pose::from_position(Vec3::new(0.0, 0.0, 0.0)),
//!     vec![EmitterSpatialState::new(
//!         emitter,
//!         Pose::from_position(Vec3::new(5.0, 0.0, 0.0)),
//!     )],
//! ))?;
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
//! - **[`PetalSonicEvent`]**: Pull-based controlled-playback and runtime state events
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
//! - Native HRTF and Ambisonics rendering for high-quality spatialization
//! - Support for both spatial and non-spatial audio sources
//! - Real-time safe audio processing
//! - Automatic resampling to world sample rate
//! - Loop modes: once or infinite
//! - Event-driven architecture for playback notifications
//! - Performance profiling via timing events

mod acoustics;
mod audio_data;
mod config;
mod domain;
mod engine;
mod error;
mod events;
mod gain;
mod math;
mod mixer;
mod platform;
mod playback;
mod spatial;
mod world;

pub use acoustics::{
    AcousticHit, AcousticMaterial, AcousticRay, AcousticSceneSnapshot, BatchedAnyHitRayTracer,
    BatchedClosestHitRayTracer,
};
pub use config::{LatencyProfile, OutputDevicePolicy, PetalSonicWorldDesc, SpatialQuality};
pub use domain::{
    Bus, BusDesc, BusParams, Emitter, EmitterDesc, EmitterSpatialState, PlayOptions,
    PlaybackControl, PlaybackTag, ResidentClip, SpatialFrame,
};
pub use error::PetalSonicError;
pub use events::{
    PetalSonicEvent, RenderTimingEvent, RuntimeDiagnostics, RuntimeState, RuntimeStatus,
};
pub use math::{Pose, Quat, Vec3};
pub use world::PetalSonicWorld;
