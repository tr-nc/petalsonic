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
use crate::spatial::DirectPathOverride;
use crate::world::SourceId;
use std::sync::Arc;

/// Loop mode for audio playback
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    /// Play once and stop
    /// Emits SourceCompleted event when finished
    Once,
    /// Loop infinitely
    /// Emits SourceLooped event at the end of each iteration
    Infinite,
}

impl Default for LoopMode {
    fn default() -> Self {
        Self::Once
    }
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
    /// SourceId of the audio data being played
    pub audio_id: SourceId,
    /// Reference to the audio data
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
}

impl PlaybackInstance {
    pub fn new(
        audio_id: SourceId,
        audio_data: Arc<PetalSonicAudioData>,
        config: SourceConfig,
        loop_mode: LoopMode,
    ) -> Self {
        let total_frames = audio_data.samples().len();
        let sample_rate = audio_data.sample_rate();
        let info = PlaybackInfo::new(total_frames, sample_rate);

        Self {
            audio_id,
            audio_data,
            info,
            config,
            loop_mode,
            direct_path_override: None,
            reached_end_this_iteration: false,
        }
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
    /// # Arguments
    /// * `progress` - Playback progress in range [0.0, 1.0]
    ///
    /// # Behavior
    /// - Clamps progress to valid range
    /// - Converts progress to frame position
    /// - Updates current_frame and current_time
    /// - Clears the end flag (in case we were at the end)
    /// - Works for both playing and paused sources
    pub fn seek(&mut self, progress: f32) {
        let progress_clamped = progress.clamp(0.0, 1.0);
        let total_frames = self.audio_data.samples().len();
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
            .update_position(self.info.current_frame, self.audio_data.sample_rate());

        // Clear end flag in case we were at the end
        self.reached_end_this_iteration = false;
    }

    /// Advance playback cursor and check for completion with wraparound support
    ///
    /// This is the **single source of truth** for frame advancement and completion checking
    /// in the new wraparound implementation. Call this from fill_buffer().
    ///
    /// # Arguments
    /// * `frames_consumed` - Number of frames consumed from audio data
    ///
    /// # Behavior
    /// - Updates current_frame and timing info
    /// - If reached end of audio data:
    ///   - For LoopMode::Infinite: Wraps current_frame to beginning, keeps playing
    ///   - For LoopMode::Once: Sets state to Stopped
    ///   - Sets `reached_end_this_iteration` flag for event emission in both cases
    fn advance_and_check_completion_with_wrap(&mut self, frames_consumed: usize) {
        let total_frames = self.audio_data.samples().len();
        self.info.current_frame += frames_consumed;

        // Check if we've reached or passed the end
        if self.info.current_frame >= total_frames {
            match self.loop_mode {
                LoopMode::Infinite => {
                    // Wrap around - keep playing
                    self.info.current_frame %= total_frames;
                    // Note: reached_end_this_iteration already set in fill_buffer
                    // State remains Playing
                    log::debug!(
                        "Source {} wrapped around to frame {} (Infinite loop)",
                        self.audio_id,
                        self.info.current_frame
                    );
                }
                LoopMode::Once => {
                    // Stop playback
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
            .update_position(self.info.current_frame, self.audio_data.sample_rate());
    }

    /// Fill audio buffer for this instance
    /// Returns the number of frames actually filled
    ///
    /// # Behavior
    /// When reaching the end of audio data:
    /// - For LoopMode::Infinite: Wraps around to beginning seamlessly within the same buffer
    /// - For LoopMode::Once: Stops filling and sets state to Stopped
    pub fn fill_buffer(&mut self, buffer: &mut [f32], channels: u16) -> usize {
        if !matches!(self.info.play_state, PlayState::Playing) {
            return 0;
        }

        let channels_usize = channels as usize;
        let frame_count = buffer.len() / channels_usize;
        let samples = self.audio_data.samples();
        let total_frames = samples.len();
        let mut frames_filled = 0;

        // Get volume from config
        let volume = self.config.volume();

        for frame_idx in 0..frame_count {
            let mut sample_idx = self.info.current_frame + frame_idx;

            // Handle wraparound for infinite looping
            if sample_idx >= total_frames {
                if matches!(self.loop_mode, LoopMode::Infinite) {
                    // Mark that we reached end (for event emission)
                    if !self.reached_end_this_iteration {
                        self.reached_end_this_iteration = true;
                    }
                    // Wrap around to beginning
                    sample_idx %= total_frames;
                } else {
                    // LoopMode::Once - stop here
                    break;
                }
            }

            let sample = samples[sample_idx];

            // Fill all channels with the same sample (mono to stereo), applying volume
            for channel in 0..channels_usize {
                let buffer_idx = frame_idx * channels_usize + channel;
                if buffer_idx < buffer.len() {
                    buffer[buffer_idx] += sample * volume; // Mix into existing buffer with volume
                }
            }

            frames_filled += 1;
        }

        // Advance cursor and check for completion with wraparound
        if frames_filled > 0 {
            self.advance_and_check_completion_with_wrap(frames_filled);
        }

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
#[derive(Debug)]
pub enum PlaybackCommand {
    /// Play a source with given configuration and loop mode
    /// Carries the audio data directly to avoid requiring engine to call back into world
    Play(SourceId, Arc<PetalSonicAudioData>, SourceConfig, LoopMode),
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
