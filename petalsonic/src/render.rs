//! One logical-stereo render quantum and all mutable state it advances.
//!
//! The output engine schedules this module; it does not reach through the seam to
//! coordinate Voices, DSP processors, buffers, telemetry, or retirement.

use crate::acoustic_propagation::{AcousticResponse, AcousticVoiceInput};
use crate::audio_data::{ResamplerType, StreamingResampler};
use crate::config::{LatencyProfile, SpatialQuality};
use crate::domain::{BusParams, PlaybackControl, SpatialFrame, VoiceId};
use crate::error::{PetalSonicError, Result};
use crate::events::{PetalSonicEvent, RenderTimingEvent, RuntimeCounters, VoiceTelemetryEvent};
use crate::mixer::{self, CompletedPlayback, MixerScratch};
use crate::platform::output::StereoFrame;
use crate::playback::{PlayState, PlaybackCommand, PlaybackInstance};
use crate::realtime_latest::RealtimeConsumer;
#[cfg(test)]
use crate::realtime_latest::RealtimeLatest;
use crate::spatial::{
    AcousticResponseReplacement, RetiredSpatialSource, SpatialProcessor, SpatialProcessorConfig,
    SpatialRenderContext,
};
use crossbeam_channel::{Receiver, Sender};
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Observer, Producer, Split},
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const LOGICAL_CHANNELS: u16 = 2;
pub(crate) const MASTER_HEADROOM_DB: f32 = -6.0;

/// The single-consume owner of every endpoint required by one render quantum.
///
/// Fields stay private so only the render module can interpret or partially consume the
/// producer-to-consumer topology assembled by the engine owner.
pub(crate) struct PreparedRender {
    desc: crate::config::PetalSonicWorldDesc,
    active_voice_count: Arc<AtomicUsize>,
    retirement_sender: Sender<VoiceId>,
    spatial_frames: RealtimeConsumer<SpatialFrame>,
    acoustic_responses: RealtimeConsumer<AcousticResponse>,
    acoustic_voice_input: AcousticVoiceInput,
    acoustic_scene_version: Arc<AtomicU64>,
    environmental_acoustics_enabled: Arc<AtomicBool>,
    regular_commands: Receiver<PlaybackCommand>,
    lifecycle_commands: Receiver<PlaybackCommand>,
    event_sender: Sender<PetalSonicEvent>,
    voice_telemetry_sender: Sender<VoiceTelemetryEvent>,
    timing_sender: Sender<RenderTimingEvent>,
    counters: Arc<RuntimeCounters>,
}

impl PreparedRender {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        desc: crate::config::PetalSonicWorldDesc,
        active_voice_count: Arc<AtomicUsize>,
        retirement_sender: Sender<VoiceId>,
        spatial_frames: RealtimeConsumer<SpatialFrame>,
        acoustic_responses: RealtimeConsumer<AcousticResponse>,
        acoustic_voice_input: AcousticVoiceInput,
        acoustic_scene_version: Arc<AtomicU64>,
        environmental_acoustics_enabled: Arc<AtomicBool>,
        regular_commands: Receiver<PlaybackCommand>,
        lifecycle_commands: Receiver<PlaybackCommand>,
        event_sender: Sender<PetalSonicEvent>,
        voice_telemetry_sender: Sender<VoiceTelemetryEvent>,
        timing_sender: Sender<RenderTimingEvent>,
        counters: Arc<RuntimeCounters>,
    ) -> Self {
        Self {
            desc,
            active_voice_count,
            retirement_sender,
            spatial_frames,
            acoustic_responses,
            acoustic_voice_input,
            acoustic_scene_version,
            environmental_acoustics_enabled,
            regular_commands,
            lifecycle_commands,
            event_sender,
            voice_telemetry_sender,
            timing_sender,
            counters,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SpatialBackendPlan {
    pub(crate) use_ambisonics: bool,
}

impl SpatialBackendPlan {
    pub(crate) fn for_quality(quality: SpatialQuality) -> Self {
        match quality {
            SpatialQuality::LowLatency => Self {
                use_ambisonics: false,
            },
            SpatialQuality::Balanced | SpatialQuality::HighQuality => Self {
                use_ambisonics: true,
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RenderSchedule {
    ring_blocks: usize,
    low_water_blocks: usize,
    high_water_blocks: usize,
    normal_chunk_blocks: usize,
    catch_up_chunk_blocks: usize,
    wake_divisor: u32,
}

impl RenderSchedule {
    pub(crate) fn for_profile(profile: LatencyProfile) -> Self {
        match profile {
            LatencyProfile::Responsive => Self {
                ring_blocks: 4,
                low_water_blocks: 1,
                high_water_blocks: 2,
                normal_chunk_blocks: 1,
                catch_up_chunk_blocks: 1,
                wake_divisor: 8,
            },
            LatencyProfile::Balanced => Self {
                ring_blocks: 8,
                low_water_blocks: 2,
                high_water_blocks: 3,
                normal_chunk_blocks: 1,
                catch_up_chunk_blocks: 2,
                wake_divisor: 4,
            },
            LatencyProfile::Robust => Self {
                ring_blocks: 12,
                low_water_blocks: 3,
                high_water_blocks: 5,
                normal_chunk_blocks: 2,
                catch_up_chunk_blocks: 3,
                wake_divisor: 3,
            },
        }
    }

    pub(crate) fn wake_interval(self, block_size: usize, sample_rate: u32) -> Duration {
        let block_duration = Duration::from_secs_f64(block_size as f64 / sample_rate as f64);
        (block_duration / self.wake_divisor)
            .clamp(Duration::from_micros(250), Duration::from_millis(2))
    }
}

struct LogicalStereoOutput {
    resampler: StreamingResampler,
    producer: HeapProd<StereoFrame>,
    world_buffer: Vec<f32>,
    resampled_buffer: Vec<f32>,
}

/// The concrete, crate-private render module.
///
/// Every field mutated by a quantum lives here without an inner lock. The sole
/// outer mutex only transfers exclusive execution between the render thread and
/// the output supervisor after that thread has stopped.
pub(crate) struct RenderQuantum {
    sample_rate: u32,
    block_size: usize,
    max_voices: usize,
    schedule: RenderSchedule,
    master_gain_linear: f32,
    active_playback: HashMap<VoiceId, PlaybackInstance>,
    active_voice_count: Arc<AtomicUsize>,
    processor: SpatialProcessor,
    buses: Vec<BusParams>,
    regular_commands: Receiver<PlaybackCommand>,
    lifecycle_commands: Receiver<PlaybackCommand>,
    spatial_frames: RealtimeConsumer<SpatialFrame>,
    current_spatial_frame: Option<Arc<SpatialFrame>>,
    acoustic_responses: RealtimeConsumer<AcousticResponse>,
    acoustic_voice_input: AcousticVoiceInput,
    acoustic_scene_version: Arc<std::sync::atomic::AtomicU64>,
    retirement_sender: Sender<VoiceId>,
    event_sender: Sender<PetalSonicEvent>,
    voice_telemetry_sender: Sender<VoiceTelemetryEvent>,
    timing_sender: Sender<RenderTimingEvent>,
    counters: Arc<RuntimeCounters>,
    backend_retirement_sender: Sender<RetiredSpatialSource>,
    pending_backend_retirements: Vec<(VoiceId, RetiredSpatialSource)>,
    mixer_scratch: MixerScratch,
    completed_playbacks: Vec<CompletedPlayback>,
    render_block_index: u64,
    output: Option<LogicalStereoOutput>,
}

impl RenderQuantum {
    pub(crate) fn new(
        startup: PreparedRender,
        buses: Vec<BusParams>,
        backend_retirement_sender: Sender<RetiredSpatialSource>,
    ) -> Result<Self> {
        let PreparedRender {
            desc,
            active_voice_count,
            retirement_sender,
            spatial_frames,
            acoustic_responses,
            acoustic_voice_input,
            acoustic_scene_version,
            environmental_acoustics_enabled,
            regular_commands,
            lifecycle_commands,
            event_sender,
            voice_telemetry_sender,
            timing_sender,
            counters,
        } = startup;
        let backend_plan = SpatialBackendPlan::for_quality(desc.spatial_quality);
        let processor = SpatialProcessor::new(SpatialProcessorConfig {
            sample_rate: desc.sample_rate,
            frame_size: desc.block_size,
            max_voices: desc.max_voices,
            distance_scaler: desc.distance_scaler,
            native_hrtf_path: desc.native_hrtf_path.clone(),
            hrtf_gain: desc.hrtf_gain,
            use_ambisonics: backend_plan.use_ambisonics,
            environmental_acoustics_enabled,
        })
        .map_err(|error| PetalSonicError::BackendUnavailable {
            backend: "spatial renderer",
            reason: error.to_string(),
        })?;
        let max_voices = desc.max_voices;
        Ok(Self {
            sample_rate: desc.sample_rate,
            block_size: desc.block_size,
            max_voices,
            schedule: RenderSchedule::for_profile(desc.latency_profile),
            master_gain_linear: crate::gain::db_to_linear(MASTER_HEADROOM_DB),
            active_playback: HashMap::with_capacity(max_voices),
            active_voice_count,
            processor,
            buses,
            regular_commands,
            lifecycle_commands,
            spatial_frames,
            current_spatial_frame: None,
            acoustic_responses,
            acoustic_voice_input,
            acoustic_scene_version,
            retirement_sender,
            event_sender,
            voice_telemetry_sender,
            timing_sender,
            counters,
            backend_retirement_sender,
            pending_backend_retirements: Vec::with_capacity(max_voices),
            mixer_scratch: MixerScratch::new(max_voices),
            completed_playbacks: Vec::with_capacity(max_voices),
            render_block_index: 0,
            output: None,
        })
    }

    pub(crate) fn schedule(&self) -> RenderSchedule {
        self.schedule
    }

    pub(crate) fn connect_output(
        &mut self,
        device_sample_rate: u32,
    ) -> Result<HeapCons<StereoFrame>> {
        let ring_size = self.block_size * self.schedule.ring_blocks;
        let (producer, consumer) = HeapRb::<StereoFrame>::new(ring_size).split();
        let resampler = StreamingResampler::new(
            self.sample_rate,
            device_sample_rate,
            LOGICAL_CHANNELS,
            self.block_size,
            Some(ResamplerType::Fast),
        )?;
        self.output = Some(LogicalStereoOutput {
            resampler,
            producer,
            world_buffer: vec![0.0; self.block_size * LOGICAL_CHANNELS as usize],
            resampled_buffer: vec![
                0.0;
                ((self.block_size as f64 * device_sample_rate as f64 / self.sample_rate as f64)
                    .ceil() as usize
                    + 10)
                    * LOGICAL_CHANNELS as usize
            ],
        });
        Ok(consumer)
    }

    pub(crate) fn disconnect_output(&mut self) {
        self.output = None;
    }

    pub(crate) fn render(&mut self) {
        self.flush_backend_retirements();
        self.consume_latest_spatial_frame();
        self.consume_latest_acoustic_response();
        let listener = self
            .current_spatial_frame
            .as_ref()
            .map_or_else(crate::math::Pose::default, |frame| frame.listener());
        let _ = self.processor.set_listener_pose(listener);
        self.process_commands();

        let Some(output) = self.output.as_ref() else {
            return;
        };
        let target = self.block_size * self.schedule.high_water_blocks;
        let occupied = output.producer.occupied_len();
        if occupied >= target {
            return;
        }
        let free = output.producer.vacant_len();
        let max_chunk = self.block_size
            * if occupied < self.block_size * self.schedule.low_water_blocks {
                self.schedule.catch_up_chunk_blocks
            } else {
                self.schedule.normal_chunk_blocks
            };
        let frames = free.min(target.saturating_sub(occupied)).min(max_chunk);
        if frames == 0 {
            return;
        }

        let timing = self.generate_samples(frames);
        self.counters.record_render_time(timing.total_time_us);
        if self.timing_sender.try_send(timing).is_ok() {
            RuntimeCounters::observe_high_water(
                &self.counters.timing_queue_high_water,
                self.timing_sender.len(),
            );
        } else {
            self.counters
                .dropped_timing_events
                .fetch_add(1, Ordering::Relaxed);
        }
        self.retire_completed_voices();
    }

    pub(crate) fn advance_without_output(&mut self, elapsed: Duration) {
        self.flush_backend_retirements();
        self.process_commands();
        let frames = (elapsed.as_secs_f64() * self.sample_rate as f64).floor() as usize;
        if frames == 0 {
            return;
        }
        self.completed_playbacks.clear();
        for (voice_id, instance) in &mut self.active_playback {
            if !matches!(instance.info.play_state, PlayState::Playing) {
                continue;
            }
            let bus = mixer::effective_bus_params(instance.bus_index, &self.buses);
            if bus.paused {
                continue;
            }
            instance.set_mix_parameters(bus);
            instance.advance_silently(frames);
            let _ = instance.check_and_clear_end_flag();
            if instance.should_reclaim() {
                self.completed_playbacks.push(CompletedPlayback {
                    voice_id: *voice_id,
                    emitter: instance.emitter,
                    completion_tag: instance.completion_tag,
                });
            }
        }
        self.active_playback
            .retain(|_, instance| !instance.should_reclaim());
        self.retire_completed_voices();
    }

    fn process_commands(&mut self) {
        for _ in 0..self.regular_commands.capacity().unwrap_or(1) {
            let Ok(command) = self.regular_commands.try_recv() else {
                break;
            };
            self.apply_command(command);
        }
        while let Ok(command) = self.lifecycle_commands.try_recv() {
            self.apply_command(command);
        }
    }

    fn apply_command(&mut self, command: PlaybackCommand) {
        match command {
            PlaybackCommand::Play(prepared) => {
                let (voice_id, instance, acoustic_voice) = prepared.start();
                if self.active_playback.insert(voice_id, instance).is_some() {
                    self.active_voice_count.fetch_sub(1, Ordering::AcqRel);
                }
                if let Some(acoustic_voice) = acoustic_voice {
                    self.acoustic_voice_input.activate(acoustic_voice);
                }
            }
            PlaybackCommand::PauseVoice(id) => self.with_voice(id, PlaybackInstance::pause),
            PlaybackCommand::StopVoice(id) => self.with_voice(id, PlaybackInstance::begin_fade_out),
            PlaybackCommand::SeekVoice(id, progress) => {
                self.with_voice(id, |voice| voice.seek(progress))
            }
            PlaybackCommand::ResumeVoice(id) => self.with_voice(id, PlaybackInstance::resume),
            PlaybackCommand::SetVoiceRate(id, rate) => {
                self.with_voice(id, |voice| voice.set_playback_rate(rate))
            }
            PlaybackCommand::PauseEmitter(emitter) => {
                self.for_emitter(emitter, false, PlaybackInstance::pause)
            }
            PlaybackCommand::ResumeEmitter(emitter) => {
                self.for_emitter(emitter, false, PlaybackInstance::resume)
            }
            PlaybackCommand::StopEmitter(emitter) => {
                self.for_emitter(emitter, false, PlaybackInstance::begin_fade_out)
            }
            PlaybackCommand::SeekEmitter(emitter, progress) => {
                self.for_emitter(emitter, false, |voice| voice.seek(progress))
            }
            PlaybackCommand::DestroyEmitter(emitter) => {
                self.for_emitter(emitter, true, PlaybackInstance::begin_fade_out)
            }
            PlaybackCommand::UpdateEmitter(update) => {
                let (emitter, render_state, bus_index) = update.into_parts();
                self.acoustic_voice_input
                    .update_emitter_audibility(emitter, render_state.volume_linear());
                for voice in self.active_playback.values_mut() {
                    if voice.emitter == emitter && !voice.detached {
                        voice.render_state = render_state.clone();
                        voice.bus_index = bus_index;
                    }
                }
            }
            PlaybackCommand::UpdateBus(index, params) => {
                if let Some(bus) = self.buses.get_mut(index) {
                    *bus = params;
                }
            }
            PlaybackCommand::StopAll => {
                for voice in self.active_playback.values_mut() {
                    voice.begin_fade_out();
                }
            }
        }
    }

    fn with_voice(&mut self, id: VoiceId, operation: impl FnOnce(&mut PlaybackInstance)) {
        if let Some(voice) = self.active_playback.get_mut(&id) {
            operation(voice);
        }
    }

    fn for_emitter(
        &mut self,
        emitter: crate::domain::Emitter,
        attached_only: bool,
        mut operation: impl FnMut(&mut PlaybackInstance),
    ) {
        for voice in self.active_playback.values_mut() {
            if voice.emitter == emitter && (!attached_only || !voice.detached) {
                operation(voice);
            }
        }
    }

    fn consume_latest_spatial_frame(&mut self) {
        let Some(next) = self.spatial_frames.consume() else {
            return;
        };
        Self::apply_spatial_frame_to_voices(&next, &mut self.active_playback);
        if let Some(previous) = self.current_spatial_frame.replace(next) {
            self.spatial_frames.retire(previous);
        }
    }

    fn apply_spatial_frame_to_voices(
        frame: &SpatialFrame,
        voices: &mut HashMap<VoiceId, PlaybackInstance>,
    ) {
        for voice in voices.values_mut() {
            if voice.detached {
                continue;
            }
            if let Some(spatial) = frame
                .emitters()
                .iter()
                .find(|spatial| spatial.emitter == voice.emitter)
            {
                voice.render_state.set_spatial_pose(spatial.pose);
            }
        }
    }

    fn consume_latest_acoustic_response(&mut self) {
        let Some(next) = self.acoustic_responses.consume() else {
            return;
        };
        let required_geometry_version = self.acoustic_scene_version.load(Ordering::Acquire);
        let retired = match self
            .processor
            .replace_acoustic_response_for_scene(next, required_geometry_version)
        {
            AcousticResponseReplacement::Accepted(previous) => previous,
            AcousticResponseReplacement::Rejected(rejected) => {
                self.counters
                    .acoustic_render_rejected_responses
                    .fetch_add(1, Ordering::Relaxed);
                Some(rejected)
            }
        };
        if let Some(retired) = retired {
            self.acoustic_responses.retire(retired);
        }
    }

    fn generate_samples(&mut self, samples_needed: usize) -> RenderTimingEvent {
        let total_start = Instant::now();
        let mut timing = RenderTimingEvent {
            mixing_time_us: 0,
            spatial_time_us: 0,
            direct_mixing_time_us: 0,
            spatial_source_count: 0,
            spatial_simulation_time_us: 0,
            direct_processing_time_us: 0,
            ambisonics_encoding_time_us: 0,
            ambisonics_decoding_time_us: 0,
            hrtf_rendering_time_us: 0,
            late_reverb_time_us: 0,
            early_reflection_time_us: 0,
            native_hrtf_direction_lookup_time_us: 0,
            native_hrtf_convolution_time_us: 0,
            resampling_time_us: 0,
            total_time_us: 0,
        };
        self.completed_playbacks.clear();
        let spatial_revision = self
            .current_spatial_frame
            .as_ref()
            .map_or(0, |frame| frame.revision());
        let mut total_generated = 0;
        while total_generated < samples_needed {
            let output = self
                .output
                .as_mut()
                .expect("output checked before rendering");
            let generated_before = total_generated;
            output.world_buffer.fill(0.0);
            let mixing_start = Instant::now();
            let profiling = mixer::mix_playback_instances_with_metrics(
                &mut output.world_buffer,
                LOGICAL_CHANNELS,
                &mut self.active_playback,
                Some(&mut self.processor),
                &self.buses,
                SpatialRenderContext {
                    render_block_index: self.render_block_index,
                    spatial_revision,
                },
                &mut self.mixer_scratch,
                &mut self.completed_playbacks,
            );
            self.render_block_index = self.render_block_index.wrapping_add(1);
            for event in self.mixer_scratch.drain_voice_telemetry() {
                Self::try_send_voice_telemetry(&self.voice_telemetry_sender, &self.counters, event);
            }
            timing.mixing_time_us += mixing_start.elapsed().as_micros() as u64;
            timing.direct_mixing_time_us += profiling.direct_mix_time_us;
            timing.spatial_time_us += profiling.spatial_mix_time_us;
            if let Some(metrics) = profiling.spatial_metrics {
                timing.spatial_source_count += metrics.spatial_source_count;
                timing.spatial_simulation_time_us += metrics.physics_simulation_time_us;
                timing.direct_processing_time_us += metrics.direct_processing_time_us;
                timing.ambisonics_encoding_time_us += metrics.ambisonics_encoding_time_us;
                timing.ambisonics_decoding_time_us += metrics.ambisonics_decoding_time_us;
                timing.hrtf_rendering_time_us += metrics.hrtf_rendering_time_us;
                timing.late_reverb_time_us += metrics.late_reverb_time_us;
                timing.early_reflection_time_us += metrics.early_reflection_time_us;
                timing.native_hrtf_direction_lookup_time_us +=
                    metrics.native_hrtf_direction_lookup_time_us;
                timing.native_hrtf_convolution_time_us += metrics.native_hrtf_convolution_time_us;
            }
            let resampling_start = Instant::now();
            if let Ok((frames_out, _)) = output
                .resampler
                .process_interleaved(&output.world_buffer, &mut output.resampled_buffer)
            {
                timing.resampling_time_us += resampling_start.elapsed().as_micros() as u64;
                apply_master_gain_and_limit(
                    &mut output.resampled_buffer,
                    frames_out,
                    LOGICAL_CHANNELS as usize,
                    self.master_gain_linear,
                );
                let mut pushed = 0;
                for samples in output
                    .resampled_buffer
                    .chunks_exact(LOGICAL_CHANNELS as usize)
                    .take(frames_out)
                {
                    if output
                        .producer
                        .try_push(StereoFrame {
                            left: samples[0],
                            right: samples[1],
                        })
                        .is_ok()
                    {
                        pushed += 1;
                    } else {
                        break;
                    }
                }
                total_generated += pushed;
            }
            if total_generated == generated_before {
                break;
            }
        }
        timing.total_time_us = total_start.elapsed().as_micros() as u64;
        timing
    }

    fn retire_completed_voices(&mut self) {
        let mut deferred = 0;
        for completed in &self.completed_playbacks {
            if let Some(retired) = self.processor.retire_voice(completed.voice_id)
                && let Err(error) = self.backend_retirement_sender.try_send(retired)
            {
                assert!(self.pending_backend_retirements.len() < self.max_voices);
                self.pending_backend_retirements
                    .push((completed.voice_id, error.into_inner()));
                deferred += 1;
            }
            self.acoustic_voice_input.retire(completed.voice_id);
        }
        self.active_voice_count.fetch_sub(
            self.completed_playbacks.len().saturating_sub(deferred),
            Ordering::AcqRel,
        );
        for completed in self.completed_playbacks.drain(..) {
            if let Some(tag) = completed.completion_tag {
                let _ = self.retirement_sender.try_send(completed.voice_id);
                Self::try_send_event(
                    &self.event_sender,
                    &self.counters,
                    PetalSonicEvent::PlaybackCompleted {
                        emitter: completed.emitter,
                        control: PlaybackControl {
                            world_id: completed.emitter.world_id,
                            voice_id: completed.voice_id,
                        },
                        tag,
                    },
                );
            }
        }
    }

    fn flush_backend_retirements(&mut self) {
        while let Some((voice_id, retired)) = self.pending_backend_retirements.pop() {
            if let Err(error) = self.backend_retirement_sender.try_send(retired) {
                self.pending_backend_retirements
                    .push((voice_id, error.into_inner()));
                break;
            }
            self.active_voice_count.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn try_send_event(
        sender: &Sender<PetalSonicEvent>,
        counters: &RuntimeCounters,
        event: PetalSonicEvent,
    ) {
        if sender.try_send(event).is_ok() {
            RuntimeCounters::observe_high_water(&counters.event_queue_high_water, sender.len());
        } else {
            counters.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn try_send_voice_telemetry(
        sender: &Sender<VoiceTelemetryEvent>,
        counters: &RuntimeCounters,
        event: VoiceTelemetryEvent,
    ) {
        if sender.try_send(event).is_ok() {
            RuntimeCounters::observe_high_water(
                &counters.voice_telemetry_queue_high_water,
                sender.len(),
            );
        } else {
            counters
                .dropped_voice_telemetry
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn apply_master_gain_and_limit(buffer: &mut [f32], frames_out: usize, channels: usize, gain: f32) {
    for sample in buffer.iter_mut().take(frames_out * channels) {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acoustic_propagation::AcousticVoice;
    use crate::audio_data::PetalSonicAudioData;
    use crate::config::PetalSonicWorldDesc;
    use crate::domain::{
        DirectPath, Emitter, EmitterDesc, EnvironmentSend, OcclusionProfile, PlayOptions,
        SourceExtent,
    };
    use crate::playback::prepare_test_voice;
    use crate::realtime_latest::Publisher;
    use crate::spatial::LateReverbParameters;
    use ringbuf::traits::Consumer;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    fn release_baseline(key: &str) -> u64 {
        include_str!("../perf/balanced_near_capacity.baseline")
            .lines()
            .filter_map(|line| line.split_once('='))
            .find_map(|(candidate, value)| {
                (candidate.trim() == key).then(|| {
                    value
                        .trim()
                        .parse()
                        .unwrap_or_else(|_| panic!("invalid {key} in release baseline"))
                })
            })
            .unwrap_or_else(|| panic!("missing {key} in release baseline"))
    }

    struct Harness {
        quantum: RenderQuantum,
        acoustic_responses: Publisher<AcousticResponse>,
        commands: Sender<PlaybackCommand>,
        events: crossbeam_channel::Receiver<PetalSonicEvent>,
        timing: crossbeam_channel::Receiver<RenderTimingEvent>,
    }

    fn harness(block_size: usize, max_voices: usize) -> Harness {
        let desc = PetalSonicWorldDesc {
            block_size,
            max_voices,
            event_queue_capacity: max_voices.saturating_mul(4).max(8),
            timing_queue_capacity: max_voices.max(8),
            ..PetalSonicWorldDesc::default()
        };
        let (commands, regular) = crossbeam_channel::bounded(max_voices.max(1));
        let (_lifecycle, lifecycle) = crossbeam_channel::bounded(max_voices.max(1));
        let (retirement_sender, _) = crossbeam_channel::bounded(max_voices.max(1));
        let (_, spatial_frames) = RealtimeLatest::bounded(1);
        let (acoustic_responses, acoustic_response_consumer) = RealtimeLatest::bounded(2);
        let (backend_retirement_sender, _) = crossbeam_channel::bounded(max_voices.max(1));
        let (event_sender, events) = crossbeam_channel::bounded(desc.event_queue_capacity);
        let (voice_telemetry_sender, _voice_telemetry) =
            crossbeam_channel::bounded(desc.event_queue_capacity);
        let (timing_sender, timing) = crossbeam_channel::bounded(desc.timing_queue_capacity);
        let startup = PreparedRender::new(
            desc,
            Arc::new(AtomicUsize::new(0)),
            retirement_sender,
            spatial_frames,
            acoustic_response_consumer,
            AcousticVoiceInput::isolated(max_voices.max(1)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(true)),
            regular,
            lifecycle,
            event_sender,
            voice_telemetry_sender,
            timing_sender,
            Arc::new(RuntimeCounters::default()),
        );
        Harness {
            quantum: RenderQuantum::new(
                startup,
                vec![BusParams::default()],
                backend_retirement_sender,
            )
            .unwrap(),
            acoustic_responses,
            commands,
            events,
            timing,
        }
    }

    fn empty_acoustic_response(revision: u64) -> Arc<AcousticResponse> {
        Arc::new(AcousticResponse {
            spatial_revision: revision,
            geometry_version: 0,
            direct: Vec::new(),
            late_reverb: LateReverbParameters::SILENT,
            published_at: Instant::now(),
            solve_time_us: 0,
        })
    }

    #[test]
    fn acoustic_replacements_progress_without_spatial_or_scene_publication() {
        let mut harness = harness(16, 1);
        let emitter = Emitter {
            world_id: 1,
            index: 0,
            generation: 1,
        };

        for revision in 1..=6 {
            let voice_id = VoiceId::from(revision);
            harness
                .quantum
                .acoustic_voice_input
                .activate(AcousticVoice {
                    voice_id,
                    emitter,
                    emitter_world_pose: crate::math::Pose::identity(),
                    acoustic_priority: 1.0,
                    audibility: 1.0,
                    detached: false,
                    direct_path: DirectPath::default(),
                    environment_send: EnvironmentSend::default(),
                    source_extent: SourceExtent::Point,
                    occlusion_profile: OcclusionProfile::PointExact,
                    routing_generation: 0,
                });
            harness
                .acoustic_responses
                .publish_latest(empty_acoustic_response(revision))
                .unwrap();

            harness.quantum.render();

            assert_eq!(
                harness.quantum.processor.acoustic_response_generation(),
                Some((revision, 0))
            );
            harness.quantum.acoustic_voice_input.retire(voice_id);
        }
    }

    fn play_command(
        voice_id: VoiceId,
        emitter: Emitter,
        clip: Arc<PetalSonicAudioData>,
        block_size: usize,
    ) -> PlaybackCommand {
        PlaybackCommand::Play(prepare_test_voice(
            voice_id,
            emitter,
            clip,
            EmitterDesc::non_spatial(),
            PlayOptions::looping(),
            None,
            0,
            block_size,
        ))
    }

    #[test]
    fn one_quantum_applies_commands_advances_one_cursor_and_writes_stereo() {
        let block_size = 64;
        let mut harness = harness(block_size, 4);
        let emitter = Emitter {
            world_id: 1,
            index: 0,
            generation: 1,
        };
        let clip = Arc::new(PetalSonicAudioData::new(
            vec![0.25; block_size * 8],
            48_000,
            1,
            Duration::from_secs_f64((block_size * 8) as f64 / 48_000.0),
        ));
        harness
            .quantum
            .active_voice_count
            .store(1, Ordering::Release);
        harness
            .commands
            .try_send(play_command(VoiceId::from(1), emitter, clip, block_size))
            .unwrap();
        let mut consumer = harness.quantum.connect_output(48_000).unwrap();

        harness.quantum.render();

        let frame = consumer.try_pop().expect("quantum produced stereo output");
        assert!(frame.left > 0.0 && frame.right > 0.0);
        assert_eq!(
            harness.quantum.active_playback[&VoiceId::from(1)]
                .info
                .current_frame,
            block_size * 2
        );
    }

    #[test]
    fn output_recovery_advances_the_same_voice_timeline() {
        let block_size = 64;
        let mut harness = harness(block_size, 2);
        let emitter = Emitter {
            world_id: 1,
            index: 0,
            generation: 1,
        };
        let clip = Arc::new(PetalSonicAudioData::new(
            (0..256).map(|sample| sample as f32 / 256.0).collect(),
            48_000,
            1,
            Duration::from_secs_f64(256.0 / 48_000.0),
        ));
        harness
            .quantum
            .active_voice_count
            .store(1, Ordering::Release);
        harness
            .commands
            .try_send(play_command(VoiceId::from(7), emitter, clip, block_size))
            .unwrap();

        harness
            .quantum
            .advance_without_output(Duration::from_millis(2));
        assert_eq!(
            harness.quantum.active_playback[&VoiceId::from(7)]
                .info
                .current_frame,
            96
        );
        let mut consumer = harness.quantum.connect_output(48_000).unwrap();
        harness.quantum.render();
        assert!(consumer.try_pop().is_some());
        assert_eq!(
            harness.quantum.active_playback[&VoiceId::from(7)]
                .info
                .current_frame,
            224
        );
    }

    #[test]
    fn spatial_frame_moves_only_attached_voice_and_preserves_captured_extent() {
        let mut harness = harness(32, 2);
        let emitter = Emitter {
            world_id: 1,
            index: 4,
            generation: 2,
        };
        let old_pose = crate::math::Pose::from_position(crate::math::Vec3::X);
        let new_pose = crate::math::Pose::from_position(crate::math::Vec3::Y);
        let clip = Arc::new(PetalSonicAudioData::new(
            vec![0.25; 64],
            48_000,
            1,
            Duration::from_secs_f64(64.0 / 48_000.0),
        ));
        for (id, detached) in [(1, false), (2, true)] {
            let mut options = PlayOptions::looping();
            if detached {
                options = options.detached();
            }
            let voice = prepare_test_voice(
                VoiceId::from(id),
                emitter,
                clip.clone(),
                EmitterDesc::spatial(old_pose),
                options,
                None,
                0,
                32,
            )
            .start()
            .1;
            harness
                .quantum
                .active_playback
                .insert(VoiceId::from(id), voice);
        }
        let frame = SpatialFrame::new(
            1,
            0.0,
            crate::math::Pose::default(),
            vec![crate::domain::EmitterSpatialState::new(emitter, new_pose)],
        );

        RenderQuantum::apply_spatial_frame_to_voices(&frame, &mut harness.quantum.active_playback);

        assert_eq!(
            harness.quantum.active_playback[&VoiceId::from(1)]
                .render_state
                .spatial_pose(),
            Some(new_pose)
        );
        assert_eq!(
            harness.quantum.active_playback[&VoiceId::from(2)]
                .render_state
                .spatial_pose(),
            Some(old_pose)
        );
        assert_eq!(
            harness.quantum.active_playback[&VoiceId::from(1)].source_extent,
            SourceExtent::Point
        );
    }

    #[test]
    fn warmed_quantum_is_allocation_free() {
        const VOICES: usize = 32;
        let block_size = 64;
        let mut harness = harness(block_size, VOICES);
        let source = Arc::new(PetalSonicAudioData::new(
            vec![0.25 / VOICES as f32; block_size * 16],
            48_000,
            1,
            Duration::from_secs_f64((block_size * 16) as f64 / 48_000.0),
        ));
        harness
            .quantum
            .active_voice_count
            .store(VOICES, Ordering::Release);
        for voice in 0..VOICES {
            harness
                .commands
                .try_send(play_command(
                    VoiceId::from(voice as u64 + 1),
                    Emitter {
                        world_id: 1,
                        index: voice as u32,
                        generation: 1,
                    },
                    source.clone(),
                    block_size,
                ))
                .unwrap();
        }
        let mut consumer = harness.quantum.connect_output(44_100).unwrap();
        harness.quantum.render();
        while consumer.try_pop().is_some() {}
        while harness.timing.try_recv().is_ok() {}

        let activity = crate::test_support::realtime_memory_activity(|| {
            harness.quantum.render();
        });

        assert_eq!(activity, 0, "steady render quantum allocated or freed");
        assert!(consumer.try_pop().is_some());
        assert!(harness.events.try_recv().is_err());
    }

    #[test]
    fn latency_profiles_select_only_bounded_refill_schedules() {
        let responsive = RenderSchedule::for_profile(LatencyProfile::Responsive);
        let balanced = RenderSchedule::for_profile(LatencyProfile::Balanced);
        let robust = RenderSchedule::for_profile(LatencyProfile::Robust);

        assert!(responsive.ring_blocks < balanced.ring_blocks);
        assert!(balanced.ring_blocks < robust.ring_blocks);
        for schedule in [responsive, balanced, robust] {
            assert!(schedule.low_water_blocks < schedule.high_water_blocks);
            assert!(schedule.high_water_blocks <= schedule.ring_blocks);
            assert!(schedule.catch_up_chunk_blocks <= schedule.high_water_blocks);
        }
    }

    #[test]
    fn warmed_near_capacity_quantum_meets_release_budget() {
        const VOICES: usize = 32;
        const SAMPLES: usize = 1_024;
        let block_size = 64;
        let mut harness = harness(block_size, VOICES);
        let source = Arc::new(PetalSonicAudioData::new(
            vec![0.25 / VOICES as f32; block_size * 16],
            48_000,
            1,
            Duration::from_secs_f64((block_size * 16) as f64 / 48_000.0),
        ));
        harness
            .quantum
            .active_voice_count
            .store(VOICES, Ordering::Release);
        for voice in 0..VOICES {
            harness
                .commands
                .try_send(play_command(
                    VoiceId::from(voice as u64 + 1),
                    Emitter {
                        world_id: 1,
                        index: voice as u32,
                        generation: 1,
                    },
                    source.clone(),
                    block_size,
                ))
                .unwrap();
        }
        let mut consumer = harness.quantum.connect_output(44_100).unwrap();
        harness.quantum.render();

        let mut elapsed_us = [0u64; SAMPLES];
        for elapsed in &mut elapsed_us {
            while consumer.try_pop().is_some() {}
            while harness.timing.try_recv().is_ok() {}
            let start = Instant::now();
            harness.quantum.render();
            *elapsed = start.elapsed().as_micros() as u64;
        }
        elapsed_us.sort_unstable();
        let p99 = elapsed_us[elapsed_us.len() * 99 / 100];
        let device_period_us = block_size as u64 * 1_000_000 / 48_000;
        if !cfg!(debug_assertions) {
            assert_eq!(release_baseline("voices"), VOICES as u64);
            assert_eq!(release_baseline("world_sample_rate"), 48_000);
            assert_eq!(release_baseline("device_sample_rate"), 44_100);
            assert_eq!(release_baseline("block_size"), block_size as u64);
            assert!(
                p99 * 100 < device_period_us * 80,
                "full-quantum p99 {p99}us lacks 20% device-period margin"
            );
            let limit = release_baseline("p99_us")
                .saturating_mul(100 + release_baseline("max_p99_regression_percent"))
                .div_ceil(100);
            assert!(
                p99 <= limit,
                "full-quantum p99 regressed: current={p99}us limit={limit}us"
            );
        }
    }
}
