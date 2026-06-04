//! Playback control and state management.
//!
//! This module provides types and functionality for controlling audio playback:
//! - [`LoopMode`]: Control how audio loops (once, infinite)
//! - [`PlayState`]: Current playback state (playing, paused, stopped)
//! - [`PlaybackInfo`]: Detailed playback position and timing information
//! - [`PlaybackInstance`]: Active playback instance with state management
//! - [`PlaybackCommand`]: Commands for controlling playback (internal)
//!
//! Most users will interact with playback through [`PetalSonicWorld`](crate::PetalSonicWorld)
//! methods like `play()`, `pause()`, and `stop()`, rather than using these types directly.

use crate::audio_data::PetalSonicAudioData;
use crate::config::SourceConfig;
use crate::procedural::{ProceduralAudioFactory, ProceduralAudioSource};
use crate::spatial::DirectPathOverride;
use crate::world::SourceId;
use std::fmt;
use std::sync::Arc;

/// Loop mode for audio playback
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoopMode {
    /// Play once and stop
    /// Emits SourceCompleted event when finished
    #[default]
    Once,
    /// Loop infinitely
    /// Emits SourceLooped event at the end of each iteration
    Infinite,
}

/// Represents the current playback state of an audio source.
///
/// Used to track whether an audio source is currently playing, paused, or stopped.
#[derive(Debug, Clone)]
pub enum PlayState {
    /// Audio is currently playing
    Playing,
    /// Audio is paused (retains playback position)
    Paused,
    /// Audio is stopped (playback position may be reset)
    Stopped,
}

/// Information about the current playback state of an audio source
#[derive(Debug, Clone)]
pub struct PlaybackInfo {
    /// Current playback position in frames
    pub current_frame: usize,
    /// Total number of frames in the audio
    pub total_frames: usize,
    /// Current playback time in seconds
    pub current_time: f64,
    /// Total duration in seconds
    pub total_time: f64,
    /// Current playback state
    pub play_state: PlayState,
}

impl PlaybackInfo {
    pub fn new(total_frames: usize, sample_rate: u32) -> Self {
        let total_time = total_frames as f64 / sample_rate as f64;
        Self {
            current_frame: 0,
            total_frames,
            current_time: 0.0,
            total_time,
            play_state: PlayState::Stopped,
        }
    }

    pub fn update_position(&mut self, current_frame: usize, sample_rate: u32) {
        self.current_frame = current_frame.min(self.total_frames);
        self.current_time = self.current_frame as f64 / sample_rate as f64;
    }

    pub fn is_finished(&self) -> bool {
        self.current_frame >= self.total_frames
    }
}

/// Registered source payload sent from the world thread to the render thread.
#[derive(Clone)]
#[doc(hidden)]
pub enum PlaybackSource {
    Static(Arc<PetalSonicAudioData>),
    Procedural {
        factory: Arc<dyn ProceduralAudioFactory>,
        sample_rate: u32,
    },
}

impl fmt::Debug for PlaybackSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Static(audio_data) => f
                .debug_struct("Static")
                .field("sample_rate", &audio_data.sample_rate())
                .field("channels", &audio_data.channels())
                .field("total_frames", &audio_data.total_frames())
                .finish(),
            Self::Procedural { sample_rate, .. } => f
                .debug_struct("Procedural")
                .field("sample_rate", sample_rate)
                .finish_non_exhaustive(),
        }
    }
}

/// Render-thread-owned playback content.
pub enum PlaybackContent {
    Static(Arc<PetalSonicAudioData>),
    Procedural(Box<dyn ProceduralAudioSource>),
}

impl fmt::Debug for PlaybackContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Static(audio_data) => f
                .debug_struct("Static")
                .field("sample_rate", &audio_data.sample_rate())
                .field("channels", &audio_data.channels())
                .field("total_frames", &audio_data.total_frames())
                .finish(),
            Self::Procedural(_) => f.write_str("Procedural(..)"),
        }
    }
}

/// Active playback instance
#[derive(Debug)]
pub struct PlaybackInstance {
    /// SourceId of the audio data being played
    pub audio_id: SourceId,
    /// Render-thread-owned static or procedural content.
    pub content: PlaybackContent,
    /// Current playback information
    pub info: PlaybackInfo,
    /// Source configuration (spatial/non-spatial)
    pub config: SourceConfig,
    /// Loop mode for this playback
    pub loop_mode: LoopMode,
    /// Optional host-provided direct-path override used during spatial processing.
    pub direct_path_override: Option<DirectPathOverride>,
    /// Flag to track if we've reached the end this iteration (for event emission)
    pub(crate) reached_end_this_iteration: bool,
    sample_rate: u32,
    mono_scratch: Vec<f32>,
}

impl PlaybackInstance {
    pub fn new(
        audio_id: SourceId,
        audio_data: Arc<PetalSonicAudioData>,
        config: SourceConfig,
        loop_mode: LoopMode,
    ) -> Self {
        Self::from_source(audio_id, PlaybackSource::Static(audio_data), config, loop_mode)
    }

    pub(crate) fn from_source(
        audio_id: SourceId,
        source: PlaybackSource,
        config: SourceConfig,
        loop_mode: LoopMode,
    ) -> Self {
        let (content, total_frames, sample_rate) = match source {
            PlaybackSource::Static(audio_data) => {
                let total_frames = audio_data.total_frames();
                let sample_rate = audio_data.sample_rate();
                (PlaybackContent::Static(audio_data), total_frames, sample_rate)
            }
            PlaybackSource::Procedural {
                factory,
                sample_rate,
            } => (
                PlaybackContent::Procedural(factory.create(sample_rate)),
                usize::MAX,
                sample_rate,
            ),
        };
        let info = PlaybackInfo::new(total_frames, sample_rate);

        Self {
            audio_id,
            content,
            info,
            config,
            loop_mode,
            direct_path_override: None,
            reached_end_this_iteration: false,
            sample_rate,
            mono_scratch: Vec::new(),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn total_frames(&self) -> usize {
        self.info.total_frames
    }

    pub fn is_procedural(&self) -> bool {
        matches!(self.content, PlaybackContent::Procedural(_))
    }

    /// Resume playing from current position
    pub fn resume(&mut self) {
        log::debug!(
            "Source {} resuming from frame {} (loop mode: {:?})",
            self.audio_id,
            self.info.current_frame,
            self.loop_mode
        );
        self.info.play_state = PlayState::Playing;
    }

    /// Reset playback cursor to the beginning
    pub fn reset(&mut self) {
        log::debug!("Source {} resetting cursor to beginning", self.audio_id);
        self.info.current_frame = 0;
        self.info.current_time = 0.0;
        self.reached_end_this_iteration = false;
        if let PlaybackContent::Procedural(source) = &mut self.content {
            source.reset();
        }
    }

    /// Play from the beginning (reset + resume)
    pub fn play_from_beginning(&mut self) {
        log::debug!(
            "Source {} playing from beginning (loop mode: {:?})",
            self.audio_id,
            self.loop_mode
        );
        self.reset();
        self.resume();
    }

    /// Set the loop mode
    pub fn set_loop_mode(&mut self, loop_mode: LoopMode) {
        log::debug!(
            "Source {} loop mode changed: {:?} -> {:?}",
            self.audio_id,
            self.loop_mode,
            loop_mode
        );
        self.loop_mode = loop_mode;
    }

    /// Pause this instance
    pub fn pause(&mut self) {
        log::debug!(
            "Source {} paused at frame {}",
            self.audio_id,
            self.info.current_frame
        );
        self.info.play_state = PlayState::Paused;
    }

    /// Stop this instance (keeps current position)
    pub fn stop(&mut self) {
        log::debug!(
            "Source {} stopped at frame {}",
            self.audio_id,
            self.info.current_frame
        );
        self.info.play_state = PlayState::Stopped;
    }

    /// Seek to a specific progress position (0.0 = start, 1.0 = end)
    ///
    /// Procedural sources are unbounded streams. Seeking them resets generator
    /// state and playback time to the beginning.
    pub fn seek(&mut self, progress: f32) {
        let progress_clamped = progress.clamp(0.0, 1.0);

        match &mut self.content {
            PlaybackContent::Static(audio_data) => {
                let total_frames = audio_data.total_frames();
                let target_frame = (total_frames as f32 * progress_clamped) as usize;

                log::debug!(
                    "Source {} seeking to progress {:.2}% (frame {}/{})",
                    self.audio_id,
                    progress_clamped * 100.0,
                    target_frame,
                    total_frames
                );

                self.info.current_frame = target_frame.min(total_frames);
                self.info
                    .update_position(self.info.current_frame, audio_data.sample_rate());
            }
            PlaybackContent::Procedural(source) => {
                log::debug!(
                    "Source {} resetting procedural stream for seek to {:.2}%",
                    self.audio_id,
                    progress_clamped * 100.0
                );
                source.reset();
                self.info.current_frame = 0;
                self.info.current_time = 0.0;
            }
        }

        self.reached_end_this_iteration = false;
    }

    fn advance_static(&mut self, frames_consumed: usize) {
        let total_frames = match &self.content {
            PlaybackContent::Static(audio_data) => audio_data.total_frames(),
            PlaybackContent::Procedural(_) => return,
        };

        if total_frames == 0 {
            self.reached_end_this_iteration = true;
            self.info.play_state = PlayState::Stopped;
            return;
        }

        self.info.current_frame = self.info.current_frame.saturating_add(frames_consumed);

        if self.info.current_frame >= total_frames {
            match self.loop_mode {
                LoopMode::Infinite => {
                    self.info.current_frame %= total_frames;
                    log::debug!(
                        "Source {} wrapped around to frame {} (Infinite loop)",
                        self.audio_id,
                        self.info.current_frame
                    );
                }
                LoopMode::Once => {
                    self.reached_end_this_iteration = true;
                    self.info.play_state = PlayState::Stopped;
                    log::debug!(
                        "Source {} reached end at frame {}/{} (Once mode)",
                        self.audio_id,
                        self.info.current_frame,
                        total_frames
                    );
                }
            }
        }

        self.info
            .update_position(self.info.current_frame, self.sample_rate);
    }

    fn advance_procedural(&mut self, frames_consumed: usize) {
        self.info.current_frame = self.info.current_frame.saturating_add(frames_consumed);
        self.info
            .update_position(self.info.current_frame, self.sample_rate);
    }

    /// Fill a mono buffer for this instance and apply `volume`.
    pub fn fill_mono_buffer(&mut self, buffer: &mut [f32], volume: f32) -> usize {
        buffer.fill(0.0);

        if !matches!(self.info.play_state, PlayState::Playing) {
            return 0;
        }

        match &mut self.content {
            PlaybackContent::Static(audio_data) => {
                let samples = audio_data.samples();
                let channels = audio_data.channels().max(1) as usize;
                let total_frames = audio_data.total_frames();
                if total_frames == 0 {
                    self.reached_end_this_iteration = true;
                    self.info.play_state = PlayState::Stopped;
                    return 0;
                }

                let current_frame = self.info.current_frame;
                let mut frames_filled = 0;

                for (frame_idx, out_sample) in buffer.iter_mut().enumerate() {
                    let mut source_frame = current_frame + frame_idx;

                    if source_frame >= total_frames {
                        if matches!(self.loop_mode, LoopMode::Infinite) {
                            if !self.reached_end_this_iteration {
                                self.reached_end_this_iteration = true;
                            }
                            source_frame %= total_frames;
                        } else {
                            break;
                        }
                    }

                    let base_idx = source_frame * channels;
                    let mut mono = 0.0;
                    for channel in 0..channels {
                        mono += samples.get(base_idx + channel).copied().unwrap_or(0.0);
                    }
                    *out_sample = (mono / channels as f32) * volume;
                    frames_filled += 1;
                }

                if frames_filled > 0 {
                    self.advance_static(frames_filled);
                }

                frames_filled
            }
            PlaybackContent::Procedural(source) => {
                source.render_mono(buffer);
                if volume != 1.0 {
                    for sample in buffer.iter_mut() {
                        *sample *= volume;
                    }
                }
                let frames_filled = buffer.len();
                self.advance_procedural(frames_filled);
                frames_filled
            }
        }
    }

    /// Fill audio buffer for this instance.
    /// Returns the number of frames actually filled.
    pub fn fill_buffer(&mut self, buffer: &mut [f32], channels: u16) -> usize {
        let channels_usize = channels as usize;
        if channels_usize == 0 {
            return 0;
        }
        let frame_count = buffer.len() / channels_usize;
        if frame_count == 0 {
            return 0;
        }

        let mut scratch = std::mem::take(&mut self.mono_scratch);
        if scratch.len() < frame_count {
            scratch.resize(frame_count, 0.0);
        }

        let volume = self.config.volume();
        let frames_filled = self.fill_mono_buffer(&mut scratch[..frame_count], volume);

        for frame_idx in 0..frames_filled {
            let sample = scratch[frame_idx];
            for channel in 0..channels_usize {
                let buffer_idx = frame_idx * channels_usize + channel;
                if buffer_idx < buffer.len() {
                    buffer[buffer_idx] += sample;
                }
            }
        }

        self.mono_scratch = scratch;
        frames_filled
    }

    /// Check if this instance reached the end of playback this iteration
    /// Returns true if reached end, and also returns the loop mode for event determination
    /// This is used by the mixer to emit appropriate events
    pub fn check_and_clear_end_flag(&mut self) -> Option<LoopMode> {
        if self.reached_end_this_iteration {
            self.reached_end_this_iteration = false;
            Some(self.loop_mode)
        } else {
            None
        }
    }
}

/// Commands that can be sent to the audio engine for playback control.
///
/// These commands are used internally to communicate between the main thread
/// and the audio processing thread. Most users will interact with playback
/// through [`PetalSonicWorld`](crate::PetalSonicWorld) methods instead.
///
/// # Variants
///
/// - `Play`: Start playing an audio source with specified configuration and loop mode
/// - `Pause`: Pause a playing audio source
/// - `Stop`: Stop an audio source and reset its position
/// - `StopAll`: Stop all currently playing audio sources
/// - `UpdateConfig`: Update the spatial configuration of a playing source
/// - `UpdateDirectPathOverride`: Update host-provided direct-path data for a playing source
/// - `Seek`: Seek to a specific position in the audio (0.0 = start, 1.0 = end)
pub enum PlaybackCommand {
    /// Play a source with given configuration and loop mode.
    /// Carries the source payload directly to avoid requiring engine to call back into world.
    Play(SourceId, PlaybackSource, SourceConfig, LoopMode),
    /// Pause a specific source
    Pause(SourceId),
    /// Stop a specific source
    Stop(SourceId),
    /// Stop all playing sources
    StopAll,
    /// Update the configuration of a source
    UpdateConfig(SourceId, SourceConfig),
    /// Update host-provided direct-path override data for a source.
    UpdateDirectPathOverride(SourceId, Option<DirectPathOverride>),
    /// Seek to a specific position (progress in range [0.0, 1.0])
    Seek(SourceId, f32),
}

impl fmt::Debug for PlaybackCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Play(source_id, source, config, loop_mode) => f
                .debug_tuple("Play")
                .field(source_id)
                .field(source)
                .field(config)
                .field(loop_mode)
                .finish(),
            Self::Pause(source_id) => f.debug_tuple("Pause").field(source_id).finish(),
            Self::Stop(source_id) => f.debug_tuple("Stop").field(source_id).finish(),
            Self::StopAll => f.write_str("StopAll"),
            Self::UpdateConfig(source_id, config) => f
                .debug_tuple("UpdateConfig")
                .field(source_id)
                .field(config)
                .finish(),
            Self::UpdateDirectPathOverride(source_id, direct_path_override) => f
                .debug_tuple("UpdateDirectPathOverride")
                .field(source_id)
                .field(direct_path_override)
                .finish(),
            Self::Seek(source_id, progress) => f
                .debug_tuple("Seek")
                .field(source_id)
                .field(progress)
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SourceConfig;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSource {
        next: f32,
        resets: Arc<AtomicUsize>,
    }

    impl ProceduralAudioSource for CountingSource {
        fn render_mono(&mut self, out: &mut [f32]) {
            for sample in out {
                *sample = self.next;
                self.next += 1.0;
            }
        }

        fn reset(&mut self) {
            self.resets.fetch_add(1, Ordering::Relaxed);
            self.next = 0.0;
        }
    }

    struct CountingFactory {
        resets: Arc<AtomicUsize>,
    }

    impl ProceduralAudioFactory for CountingFactory {
        fn create(&self, _sample_rate: u32) -> Box<dyn ProceduralAudioSource> {
            Box::new(CountingSource {
                next: 0.0,
                resets: self.resets.clone(),
            })
        }
    }

    #[test]
    fn procedural_source_fills_non_spatial_buffer_and_resets() {
        let resets = Arc::new(AtomicUsize::new(0));
        let mut instance = PlaybackInstance::from_source(
            SourceId::from(7),
            PlaybackSource::Procedural {
                factory: Arc::new(CountingFactory {
                    resets: resets.clone(),
                }),
                sample_rate: 48_000,
            },
            SourceConfig::non_spatial(),
            LoopMode::Infinite,
        );

        instance.play_from_beginning();
        assert_eq!(resets.load(Ordering::Relaxed), 1);

        let mut stereo = [0.0; 8];
        let frames = instance.fill_buffer(&mut stereo, 2);
        assert_eq!(frames, 4);
        assert_eq!(stereo, [0.0, 0.0, 1.0, 1.0, 2.0, 2.0, 3.0, 3.0]);
        assert_eq!(instance.info.current_frame, 4);

        instance.seek(0.5);
        assert_eq!(resets.load(Ordering::Relaxed), 2);
        assert_eq!(instance.info.current_frame, 0);

        let mut mono = [0.0; 3];
        let frames = instance.fill_mono_buffer(&mut mono, 0.5);
        assert_eq!(frames, 3);
        assert_eq!(mono, [0.0, 0.5, 1.0]);
    }
}
