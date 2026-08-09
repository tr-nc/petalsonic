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
use crate::domain::{Emitter, PlaybackTag};
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

/// Active playback instance
#[derive(Debug)]
pub struct PlaybackInstance {
    /// Internal identity of this playback voice.
    pub voice_id: SourceId,
    /// Logical emitter that initiated this voice.
    pub emitter: Emitter,
    /// Detached voices survive emitter destruction and stop following emitter updates.
    pub detached: bool,
    /// Present only for explicitly controlled playback.
    pub completion_tag: Option<PlaybackTag>,
    /// Immutable resident PCM shared by playback voices.
    pub audio_data: Arc<PetalSonicAudioData>,
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
        voice_id: SourceId,
        emitter: Emitter,
        audio_data: Arc<PetalSonicAudioData>,
        config: SourceConfig,
        loop_mode: LoopMode,
    ) -> Self {
        Self::from_source(
            voice_id, emitter, audio_data, config, loop_mode, false, None,
        )
    }

    pub(crate) fn from_source(
        voice_id: SourceId,
        emitter: Emitter,
        audio_data: Arc<PetalSonicAudioData>,
        config: SourceConfig,
        loop_mode: LoopMode,
        detached: bool,
        completion_tag: Option<PlaybackTag>,
    ) -> Self {
        let total_frames = audio_data.total_frames();
        let sample_rate = audio_data.sample_rate();
        let info = PlaybackInfo::new(total_frames, sample_rate);

        Self {
            voice_id,
            emitter,
            detached,
            completion_tag,
            audio_data,
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

    /// Resume playing from current position
    pub fn resume(&mut self) {
        log::debug!(
            "Source {} resuming from frame {} (loop mode: {:?})",
            self.voice_id,
            self.info.current_frame,
            self.loop_mode
        );
        self.info.play_state = PlayState::Playing;
    }

    /// Reset playback cursor to the beginning
    pub fn reset(&mut self) {
        log::debug!("Source {} resetting cursor to beginning", self.voice_id);
        self.info.current_frame = 0;
        self.info.current_time = 0.0;
        self.reached_end_this_iteration = false;
    }

    /// Play from the beginning (reset + resume)
    pub fn play_from_beginning(&mut self) {
        log::debug!(
            "Source {} playing from beginning (loop mode: {:?})",
            self.voice_id,
            self.loop_mode
        );
        self.reset();
        self.resume();
    }

    /// Set the loop mode
    pub fn set_loop_mode(&mut self, loop_mode: LoopMode) {
        log::debug!(
            "Source {} loop mode changed: {:?} -> {:?}",
            self.voice_id,
            self.loop_mode,
            loop_mode
        );
        self.loop_mode = loop_mode;
    }

    /// Pause this instance
    pub fn pause(&mut self) {
        log::debug!(
            "Source {} paused at frame {}",
            self.voice_id,
            self.info.current_frame
        );
        self.info.play_state = PlayState::Paused;
    }

    /// Stop this instance (keeps current position)
    pub fn stop(&mut self) {
        log::debug!(
            "Source {} stopped at frame {}",
            self.voice_id,
            self.info.current_frame
        );
        self.info.play_state = PlayState::Stopped;
    }

    /// Seek to a specific progress position (0.0 = start, 1.0 = end)
    ///
    pub fn seek(&mut self, progress: f32) {
        let progress_clamped = progress.clamp(0.0, 1.0);
        let total_frames = self.audio_data.total_frames();
        let target_frame = (total_frames as f32 * progress_clamped) as usize;

        log::debug!(
            "Source {} seeking to progress {:.2}% (frame {}/{})",
            self.voice_id,
            progress_clamped * 100.0,
            target_frame,
            total_frames
        );

        self.info.current_frame = target_frame.min(total_frames);
        self.info
            .update_position(self.info.current_frame, self.audio_data.sample_rate());

        self.reached_end_this_iteration = false;
    }

    fn advance_static(&mut self, frames_consumed: usize) {
        let total_frames = self.audio_data.total_frames();

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
                        self.voice_id,
                        self.info.current_frame
                    );
                }
                LoopMode::Once => {
                    self.reached_end_this_iteration = true;
                    self.info.play_state = PlayState::Stopped;
                    log::debug!(
                        "Source {} reached end at frame {}/{} (Once mode)",
                        self.voice_id,
                        self.info.current_frame,
                        total_frames
                    );
                }
            }
        }

        self.info
            .update_position(self.info.current_frame, self.sample_rate);
    }

    /// Fill a mono buffer for this instance and apply `volume`.
    pub fn fill_mono_buffer(&mut self, buffer: &mut [f32], volume: f32) -> usize {
        buffer.fill(0.0);

        if !matches!(self.info.play_state, PlayState::Playing) {
            return 0;
        }

        let samples = self.audio_data.samples();
        let channels = self.audio_data.channels().max(1) as usize;
        let total_frames = self.audio_data.total_frames();
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

        for (frame_idx, sample) in scratch.iter().copied().take(frames_filled).enumerate() {
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
    Play {
        voice_id: SourceId,
        emitter: Emitter,
        source: Arc<PetalSonicAudioData>,
        config: SourceConfig,
        loop_mode: LoopMode,
        detached: bool,
        completion_tag: Option<PlaybackTag>,
    },
    PauseVoice(SourceId),
    StopVoice(SourceId),
    SeekVoice(SourceId, f32),
    PauseEmitter(Emitter),
    StopEmitter(Emitter),
    SeekEmitter(Emitter, f32),
    DestroyEmitter(Emitter),
    StopAll,
    UpdateEmitter(Emitter, SourceConfig),
    UpdateDirectPathOverride(Emitter, Option<DirectPathOverride>),
}

impl fmt::Debug for PlaybackCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Play {
                voice_id,
                emitter,
                source,
                config,
                loop_mode,
                detached,
                completion_tag,
            } => f
                .debug_struct("Play")
                .field("voice_id", voice_id)
                .field("emitter", emitter)
                .field("source", source)
                .field("config", config)
                .field("loop_mode", loop_mode)
                .field("detached", detached)
                .field("completion_tag", completion_tag)
                .finish(),
            Self::PauseVoice(voice_id) => f.debug_tuple("PauseVoice").field(voice_id).finish(),
            Self::StopVoice(voice_id) => f.debug_tuple("StopVoice").field(voice_id).finish(),
            Self::SeekVoice(voice_id, progress) => f
                .debug_tuple("SeekVoice")
                .field(voice_id)
                .field(progress)
                .finish(),
            Self::PauseEmitter(emitter) => f.debug_tuple("PauseEmitter").field(emitter).finish(),
            Self::StopEmitter(emitter) => f.debug_tuple("StopEmitter").field(emitter).finish(),
            Self::SeekEmitter(emitter, progress) => f
                .debug_tuple("SeekEmitter")
                .field(emitter)
                .field(progress)
                .finish(),
            Self::DestroyEmitter(emitter) => {
                f.debug_tuple("DestroyEmitter").field(emitter).finish()
            }
            Self::StopAll => f.write_str("StopAll"),
            Self::UpdateEmitter(emitter, config) => f
                .debug_tuple("UpdateEmitter")
                .field(emitter)
                .field(config)
                .finish(),
            Self::UpdateDirectPathOverride(emitter, direct_path_override) => f
                .debug_tuple("UpdateDirectPathOverride")
                .field(emitter)
                .field(direct_path_override)
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SourceConfig;
    use std::time::Duration;

    #[test]
    fn resident_clip_fills_stereo_and_seeks() {
        let audio = Arc::new(PetalSonicAudioData::new(
            vec![0.0, 1.0, 2.0, 3.0],
            48_000,
            1,
            Duration::from_secs_f64(4.0 / 48_000.0),
        ));
        let mut instance = PlaybackInstance::from_source(
            SourceId::from(7),
            Emitter {
                index: 0,
                generation: 1,
            },
            audio,
            SourceConfig::non_spatial(),
            LoopMode::Infinite,
            false,
            None,
        );

        instance.play_from_beginning();

        let mut stereo = [0.0; 8];
        let frames = instance.fill_buffer(&mut stereo, 2);
        assert_eq!(frames, 4);
        assert_eq!(stereo, [0.0, 0.0, 1.0, 1.0, 2.0, 2.0, 3.0, 3.0]);
        assert_eq!(instance.info.current_frame, 0);

        instance.seek(0.5);
        assert_eq!(instance.info.current_frame, 2);

        let mut mono = [0.0; 2];
        let frames = instance.fill_mono_buffer(&mut mono, 0.5);
        assert_eq!(frames, 2);
        assert_eq!(mono, [1.0, 1.5]);
    }
}
