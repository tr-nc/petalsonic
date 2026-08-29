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
use crate::domain::{
    BusParams, DirectPath, Emitter, EnvironmentOrigin, EnvironmentSend, OcclusionProfile,
    PlayCommandId, PlaybackTag, SourceExtent, VoiceId,
};
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
    /// Current playback state
    pub play_state: PlayState,
}

impl PlaybackInfo {
    pub fn new(total_frames: usize) -> Self {
        Self {
            current_frame: 0,
            total_frames,
            current_time: 0.0,
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

pub(crate) struct VoiceStart {
    pub emitter: Emitter,
    pub audio_data: Arc<PetalSonicAudioData>,
    pub config: SourceConfig,
    pub loop_mode: LoopMode,
    pub bus_index: usize,
    pub playback_rate: f32,
    pub detached: bool,
    pub completion_tag: Option<PlaybackTag>,
    pub direct_path: DirectPath,
    pub environment_send: EnvironmentSend,
    pub play_command_id: Option<PlayCommandId>,
    pub source_extent: SourceExtent,
    pub occlusion_profile: OcclusionProfile,
    pub mono_scratch: Vec<f32>,
}

/// Active playback instance
#[derive(Debug)]
pub struct PlaybackInstance {
    /// Logical emitter that initiated this voice.
    pub emitter: Emitter,
    /// Detached voices survive emitter destruction and stop following emitter updates.
    pub detached: bool,
    /// Present only for explicitly controlled playback.
    pub completion_tag: Option<PlaybackTag>,
    /// Immutable direct-path semantics captured when this Voice was created.
    pub direct_path: DirectPath,
    /// Immutable environment routing captured when this Voice was created.
    pub environment_send: EnvironmentSend,
    /// Immutable local source domain captured when this Voice was created.
    pub source_extent: SourceExtent,
    /// Immutable geometry-response policy, orthogonal to extent and route placement.
    pub occlusion_profile: OcclusionProfile,
    play_command_id: Option<PlayCommandId>,
    first_render_pending: bool,
    environment_response_pending: bool,
    /// Immutable resident PCM shared by playback voices.
    pub audio_data: Arc<PetalSonicAudioData>,
    /// Current playback information
    pub info: PlaybackInfo,
    /// Source configuration (spatial/non-spatial)
    pub config: SourceConfig,
    /// Loop mode for this playback
    pub loop_mode: LoopMode,
    /// Fixed bus route selected when the Voice is created. Zero is Master.
    pub bus_index: usize,
    /// Flag to track if we've reached the end this iteration (for event emission)
    pub(crate) reached_end_this_iteration: bool,
    sample_rate: u32,
    cursor: f64,
    voice_rate: f32,
    mix_gain_linear: f32,
    mix_rate: f32,
    mono_scratch: Vec<f32>,
    fade_out_remaining_frames: usize,
    fade_out_total_frames: usize,
    retired: bool,
}

impl PlaybackInstance {
    pub(crate) fn from_voice(start: VoiceStart) -> Self {
        let VoiceStart {
            emitter,
            audio_data,
            config,
            loop_mode,
            bus_index,
            playback_rate,
            detached,
            completion_tag,
            direct_path,
            environment_send,
            play_command_id,
            source_extent,
            occlusion_profile,
            mono_scratch,
        } = start;
        let total_frames = audio_data.total_frames();
        let sample_rate = audio_data.sample_rate();
        let info = PlaybackInfo::new(total_frames);

        Self {
            emitter,
            detached,
            completion_tag,
            direct_path,
            environment_send,
            source_extent,
            occlusion_profile,
            play_command_id,
            first_render_pending: play_command_id.is_some(),
            environment_response_pending: play_command_id.is_some()
                && !matches!(environment_send.origin(), EnvironmentOrigin::Disabled),
            audio_data,
            info,
            config,
            loop_mode,
            bus_index,
            reached_end_this_iteration: false,
            sample_rate,
            cursor: 0.0,
            voice_rate: playback_rate,
            mix_gain_linear: 1.0,
            mix_rate: playback_rate,
            mono_scratch,
            fade_out_remaining_frames: 0,
            fade_out_total_frames: 0,
            retired: false,
        }
    }

    /// Resume playing from current position
    pub fn resume(&mut self) {
        self.info.play_state = PlayState::Playing;
    }

    /// Reset playback cursor to the beginning
    pub fn reset(&mut self) {
        self.info.current_frame = 0;
        self.info.current_time = 0.0;
        self.cursor = 0.0;
        self.reached_end_this_iteration = false;
    }

    /// Play from the beginning (reset + resume)
    pub fn play_from_beginning(&mut self) {
        self.reset();
        self.retired = false;
        self.fade_out_remaining_frames = 0;
        self.fade_out_total_frames = 0;
        self.resume();
    }

    /// Retire an audible voice using a short, bounded de-click ramp.
    pub(crate) fn begin_fade_out(&mut self) {
        // Explicit stop/destroy does not report a natural-completion event.
        self.completion_tag = None;
        if !matches!(self.info.play_state, PlayState::Playing) {
            self.info.play_state = PlayState::Stopped;
            self.retired = true;
            return;
        }
        let fade_frames = (self.sample_rate as usize / 200).max(1);
        self.fade_out_total_frames = fade_frames;
        self.fade_out_remaining_frames = fade_frames;
    }

    /// Pause this instance
    pub fn pause(&mut self) {
        self.info.play_state = PlayState::Paused;
    }

    /// Seek to a specific progress position (0.0 = start, 1.0 = end)
    ///
    pub fn seek(&mut self, progress: f32) {
        let progress_clamped = progress.clamp(0.0, 1.0);
        let total_frames = self.audio_data.total_frames();
        let target_frame = (total_frames as f32 * progress_clamped) as usize;

        self.info.current_frame = target_frame.min(total_frames);
        self.cursor = self.info.current_frame as f64;
        self.info
            .update_position(self.info.current_frame, self.audio_data.sample_rate());

        self.reached_end_this_iteration = false;
    }

    pub(crate) fn set_mix_parameters(&mut self, bus: BusParams) {
        self.mix_gain_linear = if bus.muted {
            0.0
        } else {
            crate::gain::db_to_linear(bus.gain_db)
        };
        self.mix_rate = self.voice_rate * bus.playback_rate;
    }

    pub(crate) fn set_playback_rate(&mut self, playback_rate: f32) {
        self.voice_rate = playback_rate;
    }

    pub(crate) fn advance_silently(&mut self, output_frames: usize) {
        if output_frames == 0 || !matches!(self.info.play_state, PlayState::Playing) {
            return;
        }
        let total_frames = self.audio_data.total_frames();
        if total_frames == 0 {
            self.reached_end_this_iteration = true;
            self.info.play_state = PlayState::Stopped;
            return;
        }

        if self.fade_out_remaining_frames > 0 {
            self.fade_out_remaining_frames =
                self.fade_out_remaining_frames.saturating_sub(output_frames);
            if self.fade_out_remaining_frames == 0 {
                self.info.play_state = PlayState::Stopped;
                self.retired = true;
            }
        }

        self.cursor += output_frames as f64 * self.mix_rate as f64;
        match self.loop_mode {
            LoopMode::Infinite => {
                if self.cursor >= total_frames as f64 {
                    self.cursor %= total_frames as f64;
                    self.reached_end_this_iteration = true;
                }
                self.info
                    .update_position(self.cursor.floor() as usize, self.sample_rate);
            }
            LoopMode::Once => {
                if self.cursor >= total_frames as f64 {
                    self.cursor = total_frames as f64;
                    self.info.update_position(total_frames, self.sample_rate);
                    self.info.play_state = PlayState::Stopped;
                    self.reached_end_this_iteration = true;
                } else {
                    self.info
                        .update_position(self.cursor.floor() as usize, self.sample_rate);
                }
            }
        }
    }

    fn next_source_frame(&mut self) -> Option<usize> {
        let total_frames = self.audio_data.total_frames();

        if total_frames == 0 {
            self.reached_end_this_iteration = true;
            self.info.play_state = PlayState::Stopped;
            return None;
        }

        if self.cursor >= total_frames as f64 {
            match self.loop_mode {
                LoopMode::Infinite => {
                    self.cursor %= total_frames as f64;
                    self.reached_end_this_iteration = true;
                }
                LoopMode::Once => {
                    self.info.update_position(total_frames, self.sample_rate);
                    self.info.play_state = PlayState::Stopped;
                    self.reached_end_this_iteration = true;
                    return None;
                }
            }
        }

        let source_frame = self.cursor.floor() as usize;
        self.cursor += self.mix_rate as f64;
        if self.cursor >= total_frames as f64 {
            match self.loop_mode {
                LoopMode::Infinite => {
                    self.cursor %= total_frames as f64;
                    self.reached_end_this_iteration = true;
                }
                LoopMode::Once => {
                    self.reached_end_this_iteration = true;
                    self.info.play_state = PlayState::Stopped;
                }
            }
        }

        let next_frame = if matches!(self.loop_mode, LoopMode::Once)
            && !matches!(self.info.play_state, PlayState::Playing)
        {
            total_frames
        } else {
            self.cursor.floor() as usize
        };
        self.info.update_position(next_frame, self.sample_rate);
        Some(source_frame)
    }

    /// Fill a mono buffer for this instance and apply `volume`.
    pub fn fill_mono_buffer(&mut self, buffer: &mut [f32], volume: f32) -> usize {
        buffer.fill(0.0);

        if !matches!(self.info.play_state, PlayState::Playing) {
            return 0;
        }

        let channels = self.audio_data.channels().max(1) as usize;
        let total_frames = self.audio_data.total_frames();
        if total_frames == 0 {
            self.reached_end_this_iteration = true;
            self.info.play_state = PlayState::Stopped;
            return 0;
        }

        let mut frames_filled = 0;

        for out_sample in buffer.iter_mut() {
            let Some(source_frame) = self.next_source_frame() else {
                break;
            };

            let base_idx = source_frame * channels;
            let mut mono = 0.0;
            for channel in 0..channels {
                mono += self
                    .audio_data
                    .samples()
                    .get(base_idx + channel)
                    .copied()
                    .unwrap_or(0.0);
            }
            let fade_gain = if self.fade_out_remaining_frames > 0 {
                let gain = self.fade_out_remaining_frames as f32
                    / self.fade_out_total_frames.max(1) as f32;
                self.fade_out_remaining_frames -= 1;
                if self.fade_out_remaining_frames == 0 {
                    self.info.play_state = PlayState::Stopped;
                    self.retired = true;
                }
                gain
            } else {
                1.0
            };
            *out_sample = (mono / channels as f32) * volume * self.mix_gain_linear * fade_gain;
            frames_filled += 1;
            if self.retired {
                break;
            }
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
    /// Returns the loop mode when this voice crossed its end boundary.
    pub fn check_and_clear_end_flag(&mut self) -> Option<LoopMode> {
        if self.reached_end_this_iteration {
            self.reached_end_this_iteration = false;
            Some(self.loop_mode)
        } else {
            None
        }
    }

    pub(crate) fn should_reclaim(&self) -> bool {
        self.retired || matches!(self.loop_mode, LoopMode::Once) && self.info.is_finished()
    }

    pub(crate) fn take_first_render_command_id(&mut self) -> Option<PlayCommandId> {
        if !self.first_render_pending {
            return None;
        }
        self.first_render_pending = false;
        self.play_command_id
    }

    pub(crate) fn telemetry_command_id(&self) -> Option<PlayCommandId> {
        self.play_command_id
    }

    pub(crate) fn pending_environment_response_id(&self) -> Option<PlayCommandId> {
        self.environment_response_pending
            .then_some(self.play_command_id)
            .flatten()
    }

    pub(crate) fn mark_environment_response_reported(&mut self) {
        self.environment_response_pending = false;
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
/// - `Seek`: Seek to a specific position in the audio (0.0 = start, 1.0 = end)
// Keeping Play inline lets the render thread move its owned buffers into a Voice without
// allocating or freeing an extra command box at a render-quantum boundary.
#[allow(clippy::large_enum_variant)]
pub enum PlaybackCommand {
    Play {
        voice_id: VoiceId,
        emitter: Emitter,
        source: Arc<PetalSonicAudioData>,
        config: SourceConfig,
        loop_mode: LoopMode,
        detached: bool,
        completion_tag: Option<PlaybackTag>,
        bus_index: usize,
        playback_rate: f32,
        direct_path: DirectPath,
        environment_send: EnvironmentSend,
        play_command_id: Option<PlayCommandId>,
        source_extent: SourceExtent,
        occlusion_profile: OcclusionProfile,
        mono_scratch: Vec<f32>,
    },
    PauseVoice(VoiceId),
    StopVoice(VoiceId),
    SeekVoice(VoiceId, f32),
    ResumeVoice(VoiceId),
    SetVoiceRate(VoiceId, f32),
    PauseEmitter(Emitter),
    ResumeEmitter(Emitter),
    StopEmitter(Emitter),
    SeekEmitter(Emitter, f32),
    DestroyEmitter(Emitter),
    StopAll,
    UpdateEmitter(Emitter, SourceConfig, usize),
    UpdateBus(usize, BusParams),
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
                bus_index,
                playback_rate,
                direct_path,
                environment_send,
                play_command_id,
                source_extent,
                occlusion_profile,
                mono_scratch,
            } => f
                .debug_struct("Play")
                .field("voice_id", voice_id)
                .field("emitter", emitter)
                .field("source", source)
                .field("config", config)
                .field("loop_mode", loop_mode)
                .field("detached", detached)
                .field("completion_tag", completion_tag)
                .field("bus_index", bus_index)
                .field("playback_rate", playback_rate)
                .field("direct_path", direct_path)
                .field("environment_send", environment_send)
                .field("play_command_id", play_command_id)
                .field("source_extent", source_extent)
                .field("occlusion_profile", occlusion_profile)
                .field("mono_scratch_len", &mono_scratch.len())
                .finish(),
            Self::PauseVoice(voice_id) => f.debug_tuple("PauseVoice").field(voice_id).finish(),
            Self::StopVoice(voice_id) => f.debug_tuple("StopVoice").field(voice_id).finish(),
            Self::SeekVoice(voice_id, progress) => f
                .debug_tuple("SeekVoice")
                .field(voice_id)
                .field(progress)
                .finish(),
            Self::ResumeVoice(voice_id) => f.debug_tuple("ResumeVoice").field(voice_id).finish(),
            Self::SetVoiceRate(voice_id, rate) => f
                .debug_tuple("SetVoiceRate")
                .field(voice_id)
                .field(rate)
                .finish(),
            Self::PauseEmitter(emitter) => f.debug_tuple("PauseEmitter").field(emitter).finish(),
            Self::ResumeEmitter(emitter) => f.debug_tuple("ResumeEmitter").field(emitter).finish(),
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
            Self::UpdateEmitter(emitter, config, bus_index) => f
                .debug_tuple("UpdateEmitter")
                .field(emitter)
                .field(config)
                .field(bus_index)
                .finish(),
            Self::UpdateBus(index, params) => f
                .debug_tuple("UpdateBus")
                .field(index)
                .field(params)
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
        let mut instance = PlaybackInstance::from_voice(VoiceStart {
            emitter: Emitter {
                world_id: 1,
                index: 0,
                generation: 1,
            },
            audio_data: audio,
            config: SourceConfig::non_spatial(),
            loop_mode: LoopMode::Infinite,
            bus_index: 0,
            playback_rate: 1.0,
            detached: false,
            completion_tag: None,
            direct_path: DirectPath::default(),
            environment_send: EnvironmentSend::default(),
            play_command_id: None,
            source_extent: SourceExtent::Point,
            occlusion_profile: OcclusionProfile::PointExact,
            mono_scratch: vec![0.0; 4],
        });

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

    #[test]
    fn playback_rate_changes_source_progress_and_muted_time_still_advances() {
        let audio = Arc::new(PetalSonicAudioData::new(
            vec![0.0, 1.0, 2.0, 3.0],
            48_000,
            1,
            Duration::from_secs_f64(4.0 / 48_000.0),
        ));
        let mut instance = PlaybackInstance::from_voice(VoiceStart {
            emitter: Emitter {
                world_id: 1,
                index: 0,
                generation: 1,
            },
            audio_data: audio,
            config: SourceConfig::non_spatial(),
            loop_mode: LoopMode::Once,
            bus_index: 0,
            playback_rate: 2.0,
            detached: false,
            completion_tag: None,
            direct_path: DirectPath::default(),
            environment_send: EnvironmentSend::default(),
            play_command_id: None,
            source_extent: SourceExtent::Point,
            occlusion_profile: OcclusionProfile::PointExact,
            mono_scratch: vec![0.0; 4],
        });
        instance.play_from_beginning();
        instance.set_mix_parameters(BusParams::default());

        let mut output = [0.0; 4];
        assert_eq!(instance.fill_mono_buffer(&mut output, 1.0), 2);
        assert_eq!(output, [0.0, 2.0, 0.0, 0.0]);
        assert!(instance.info.is_finished());

        instance.reset();
        instance.resume();
        instance.set_playback_rate(1.0);
        instance.set_mix_parameters(BusParams {
            muted: true,
            ..BusParams::default()
        });
        instance.advance_silently(3);
        assert_eq!(instance.info.current_frame, 3);
    }

    #[test]
    fn explicit_stop_uses_bounded_declick_ramp_before_reclaim() {
        let audio = Arc::new(PetalSonicAudioData::new(
            vec![1.0; 1_000],
            48_000,
            1,
            Duration::from_secs_f64(1_000.0 / 48_000.0),
        ));
        let mut instance = PlaybackInstance::from_voice(VoiceStart {
            emitter: Emitter {
                world_id: 1,
                index: 0,
                generation: 1,
            },
            audio_data: audio,
            config: SourceConfig::non_spatial(),
            loop_mode: LoopMode::Infinite,
            bus_index: 0,
            playback_rate: 1.0,
            detached: false,
            completion_tag: Some(PlaybackTag(9)),
            direct_path: DirectPath::default(),
            environment_send: EnvironmentSend::default(),
            play_command_id: None,
            source_extent: SourceExtent::Point,
            occlusion_profile: OcclusionProfile::PointExact,
            mono_scratch: vec![0.0; 256],
        });
        instance.play_from_beginning();
        instance.begin_fade_out();

        let mut output = [0.0; 240];
        assert_eq!(instance.fill_mono_buffer(&mut output, 1.0), 240);
        assert!(output[0] > output[120]);
        assert!(output[120] > output[239]);
        assert!(instance.should_reclaim());
        assert_eq!(instance.completion_tag, None);
    }
}
