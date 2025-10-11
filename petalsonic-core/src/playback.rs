use crate::audio_data::PetalSonicAudioData;
use crate::config::SourceConfig;
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

/// Playback state for an audio source
#[derive(Debug, Clone)]
pub enum PlayState {
    Playing,
    Paused,
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
    /// Flag to track if we've reached the end this iteration (for event emission)
    reached_end_this_iteration: bool,
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

    /// Fill audio buffer for this instance
    /// Returns the number of frames actually filled
    ///
    /// # Behavior
    /// When reaching the end of audio data:
    /// - Sets `reached_end_this_iteration` flag for event emission
    /// - Stops playing (for BOTH Once and Infinite modes)
    /// - Infinite mode will be explicitly restarted by the mixer
    pub fn fill_buffer(&mut self, buffer: &mut [f32], channels: u16) -> usize {
        if !matches!(self.info.play_state, PlayState::Playing) {
            return 0;
        }

        let channels_usize = channels as usize;
        let frame_count = buffer.len() / channels_usize;
        let samples = self.audio_data.samples();
        let mut frames_filled = 0;

        for frame_idx in 0..frame_count {
            if self.info.current_frame >= samples.len() {
                // Reached end of audio - mark flag and STOP (no cursor wrapping!)
                self.reached_end_this_iteration = true;
                self.info.play_state = PlayState::Stopped;

                log::debug!(
                    "Source {} reached end at frame {} (loop mode: {:?}, filled {} frames)",
                    self.audio_id,
                    self.info.current_frame,
                    self.loop_mode,
                    frames_filled
                );
                break;
            }

            let sample = samples[self.info.current_frame];

            // Fill all channels with the same sample (mono to stereo)
            for channel in 0..channels_usize {
                let buffer_idx = frame_idx * channels_usize + channel;
                if buffer_idx < buffer.len() {
                    buffer[buffer_idx] += sample; // Mix into existing buffer
                }
            }

            self.info.current_frame += 1;
            frames_filled += 1;
        }

        // Update timing info
        self.info
            .update_position(self.info.current_frame, self.audio_data.sample_rate());
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

/// Commands that can be sent to the audio engine for playback control
#[derive(Debug)]
pub enum PlaybackCommand {
    Play(SourceId, SourceConfig, LoopMode),
    Pause(SourceId),
    Stop(SourceId),
    StopAll,
    UpdateConfig(SourceId, SourceConfig),
}
