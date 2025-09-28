use crate::audio_data::PetalSonicAudioData;
use std::sync::Arc;
use uuid::Uuid;

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
    /// UUID of the audio data being played
    pub audio_id: Uuid,
    /// Reference to the audio data
    pub audio_data: Arc<PetalSonicAudioData>,
    /// Current playback information
    pub info: PlaybackInfo,
}

impl PlaybackInstance {
    pub fn new(audio_id: Uuid, audio_data: Arc<PetalSonicAudioData>) -> Self {
        let total_frames = audio_data.samples().len();
        let sample_rate = audio_data.sample_rate();
        let info = PlaybackInfo::new(total_frames, sample_rate);

        Self {
            audio_id,
            audio_data,
            info,
        }
    }

    /// Start playing this instance
    pub fn play(&mut self) {
        self.info.play_state = PlayState::Playing;
    }

    /// Pause this instance
    pub fn pause(&mut self) {
        self.info.play_state = PlayState::Paused;
    }

    /// Stop this instance and reset position
    pub fn stop(&mut self) {
        self.info.play_state = PlayState::Stopped;
        self.info.current_frame = 0;
        self.info.current_time = 0.0;
    }

    /// Fill audio buffer for this instance
    /// Returns the number of frames actually filled
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
                // Reached end of audio
                self.info.play_state = PlayState::Stopped;
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
}

/// Commands that can be sent to the audio engine for playback control
#[derive(Debug)]
pub enum PlaybackCommand {
    Play(Uuid),
    Pause(Uuid),
    Stop(Uuid),
    StopAll,
}
