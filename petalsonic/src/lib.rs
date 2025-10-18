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
//! use petalsonic_core::*;
//! use std::sync::Arc;
//!
//! // Create a world configuration
//! let config = PetalSonicWorldDesc::default();
//!
//! // Create the audio world
//! let world = PetalSonicWorld::new(config.clone())?;
//!
//! // Create and start the audio engine
//! let mut engine = PetalSonicEngine::new(config, &world)?;
//! engine.start()?;
//!
//! // Load audio data
//! let audio_data = audio_data::PetalSonicAudioData::from_path("audio.wav")?;
//!
//! // Register audio with spatial configuration
//! let source_id = world.register_audio(
//!     audio_data,
//!     SourceConfig::spatial(Vec3::new(5.0, 0.0, 0.0), 1.0)
//! )?;
//!
//! // Play the audio
//! world.play(source_id, playback::LoopMode::Once)?;
//!
//! // Update listener position as your camera/player moves
//! world.set_listener_pose(Pose::from_position(Vec3::new(0.0, 0.0, 0.0)));
//!
//! // Poll for events
//! for event in engine.poll_events() {
//!     match event {
//!         PetalSonicEvent::SourceCompleted { source_id } => {
//!             println!("Audio completed: {:?}", source_id);
//!         }
//!         _ => {}
//!     }
//! }
//! # Ok::<(), PetalSonicError>(())
//! ```
//!
//! ## Key Components
//!
//! - **[`PetalSonicWorld`]**: Main API for managing audio sources and playback on the main thread
//! - **[`PetalSonicEngine`]**: Audio processing engine that runs on a dedicated thread
//! - **[`SourceConfig`]**: Configuration for spatial vs. non-spatial audio sources
//! - **[`PetalSonicAudioData`](audio_data::PetalSonicAudioData)**: Audio data loaded from files
//! - **[`PetalSonicEvent`]**: Events emitted by the engine (completion, errors, etc.)
//! - **[`RayTracer`]**: Trait for implementing custom ray tracing for occlusion/reverb
//!
//! ## Architecture
//!
//! PetalSonic uses a three-layer threading model:
//!
//! 1. **Main Thread**: Owns `PetalSonicWorld`, loads audio, sends commands
//! 2. **Render Thread**: Processes commands, spatializes audio, generates samples
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
//! - Optional ray tracing for occlusion and reverb effects
//! - Performance profiling via timing events

pub mod audio_data;
pub mod config;
pub mod engine;
pub mod error;
pub mod events;
pub mod math;
pub mod mixer;
pub mod playback;
pub mod spatial;
pub mod world;

pub use config::{PetalSonicWorldDesc, SourceConfig};
pub use engine::{AudioFillCallback, PetalSonicEngine};
pub use error::PetalSonicError;
pub use events::{PetalSonicEvent, RenderTimingEvent};
pub use playback::{PlayState, PlaybackCommand, PlaybackInfo, PlaybackInstance};
pub use world::{PetalSonicAudioListener, PetalSonicAudioSource, PetalSonicWorld, SourceId};
