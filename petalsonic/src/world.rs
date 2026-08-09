use crate::domain::{
    Emitter, EmitterDesc, PlayOptions, PlaybackControl, PlaybackTag, ResidentClip, SpatialFrame,
};
use crate::engine::{AudioOutputDeviceInfo, PetalSonicEngine};
use crate::error::{PetalSonicError, Result};
use crate::events::{PetalSonicEvent, RenderTimingEvent};
use crate::math::Pose;
use crate::playback::PlaybackCommand;
use crate::spatial::DirectPathOverride;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Internal identity for one playback voice.
///
/// IDs are never reused within a world, so a stale control cannot alias a later
/// voice even after the earlier slot has been reclaimed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(u64);

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VoiceId({})", self.0)
    }
}

impl From<u64> for SourceId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone)]
struct EmitterState {
    clip: ResidentClip,
    desc: EmitterDesc,
}

struct EmitterSlot {
    generation: u32,
    state: Option<EmitterState>,
}

struct EmitterRegistry {
    slots: Vec<EmitterSlot>,
    free: Vec<u32>,
    len: usize,
    limit: usize,
}

impl EmitterRegistry {
    fn new(limit: usize) -> Self {
        Self {
            slots: Vec::with_capacity(limit),
            free: Vec::with_capacity(limit),
            len: 0,
            limit,
        }
    }

    fn insert(&mut self, state: EmitterState) -> Result<Emitter> {
        if self.len >= self.limit {
            return Err(PetalSonicError::CapacityExceeded {
                resource: "emitter",
                limit: self.limit,
            });
        }

        self.len += 1;
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.state = Some(state);
            return Ok(Emitter {
                index,
                generation: slot.generation,
            });
        }

        let index = self.slots.len() as u32;
        self.slots.push(EmitterSlot {
            generation: 1,
            state: Some(state),
        });
        Ok(Emitter {
            index,
            generation: 1,
        })
    }

    fn get(&self, emitter: Emitter) -> Result<&EmitterState> {
        let slot = self
            .slots
            .get(emitter.index as usize)
            .ok_or(PetalSonicError::StaleEmitter)?;
        if slot.generation != emitter.generation {
            return Err(PetalSonicError::StaleEmitter);
        }
        slot.state.as_ref().ok_or(PetalSonicError::StaleEmitter)
    }

    fn get_mut(&mut self, emitter: Emitter) -> Result<&mut EmitterState> {
        let slot = self
            .slots
            .get_mut(emitter.index as usize)
            .ok_or(PetalSonicError::StaleEmitter)?;
        if slot.generation != emitter.generation {
            return Err(PetalSonicError::StaleEmitter);
        }
        slot.state.as_mut().ok_or(PetalSonicError::StaleEmitter)
    }

    fn remove(&mut self, emitter: Emitter) -> Result<EmitterState> {
        let slot = self
            .slots
            .get_mut(emitter.index as usize)
            .ok_or(PetalSonicError::StaleEmitter)?;
        if slot.generation != emitter.generation {
            return Err(PetalSonicError::StaleEmitter);
        }
        let state = slot.state.take().ok_or(PetalSonicError::StaleEmitter)?;
        slot.generation = slot.generation.wrapping_add(1).max(1);
        self.free.push(emitter.index);
        self.len -= 1;
        Ok(state)
    }

    fn apply_spatial_frame(&mut self, frame: &SpatialFrame) -> Result<()> {
        let expected = self
            .slots
            .iter()
            .filter_map(|slot| slot.state.as_ref())
            .filter(|state| state.desc.is_spatial())
            .count();
        if frame.emitters().len() != expected {
            return Err(PetalSonicError::InvalidConfiguration {
                field: "spatial_frame",
                reason: format!(
                    "expected {expected} spatial emitters, received {}",
                    frame.emitters().len()
                ),
            });
        }

        let mut seen = HashSet::with_capacity(frame.emitters().len());
        for spatial in frame.emitters() {
            if !seen.insert(spatial.emitter) {
                return Err(PetalSonicError::InvalidConfiguration {
                    field: "spatial_frame",
                    reason: format!("contains duplicate {}", spatial.emitter),
                });
            }
            let state = self.get(spatial.emitter)?;
            if !state.desc.is_spatial() {
                return Err(PetalSonicError::InvalidConfiguration {
                    field: "spatial_frame",
                    reason: format!("{} is not spatial", spatial.emitter),
                });
            }
        }

        for spatial in frame.emitters() {
            self.get_mut(spatial.emitter)?.desc.set_pose(spatial.pose);
        }
        Ok(())
    }
}

/// Main facade for audio resources, emitters, playback, events, and runtime state.
///
/// Creating a world starts its private render runtime. Callers submit bounded,
/// non-blocking intent and never drive audio progress themselves.
pub struct PetalSonicWorld {
    desc: crate::config::PetalSonicWorldDesc,
    emitters: Mutex<EmitterRegistry>,
    next_voice_id: AtomicU64,
    active_voice_count: Arc<AtomicUsize>,
    controlled_voices: Mutex<HashMap<SourceId, Emitter>>,
    retirement_receiver: Receiver<SourceId>,
    latest_spatial_frame: Arc<Mutex<Option<Arc<SpatialFrame>>>>,
    spatial_retirement_receiver: Receiver<Arc<SpatialFrame>>,
    command_sender: Sender<PlaybackCommand>,
    engine: Mutex<Option<PetalSonicEngine>>,
}

impl PetalSonicWorld {
    pub fn new(config: crate::config::PetalSonicWorldDesc) -> Result<Self> {
        Self::validate_config(&config)?;

        let (command_sender, command_receiver) =
            crossbeam_channel::bounded(config.control_queue_capacity);
        let listener_pose = Arc::new(Mutex::new(Pose::default()));
        let active_voice_count = Arc::new(AtomicUsize::new(0));
        let (retirement_sender, retirement_receiver) =
            crossbeam_channel::bounded(config.max_voices);
        let latest_spatial_frame = Arc::new(Mutex::new(None));
        let (spatial_retirement_sender, spatial_retirement_receiver) =
            crossbeam_channel::bounded(1);
        let mut engine = PetalSonicEngine::new(
            config.clone(),
            listener_pose.clone(),
            active_voice_count.clone(),
            retirement_sender,
            latest_spatial_frame.clone(),
            spatial_retirement_sender,
        )?;
        engine.start(command_receiver)?;

        Ok(Self {
            emitters: Mutex::new(EmitterRegistry::new(config.max_emitters)),
            controlled_voices: Mutex::new(HashMap::with_capacity(config.max_voices)),
            desc: config,
            next_voice_id: AtomicU64::new(0),
            active_voice_count,
            retirement_receiver,
            latest_spatial_frame,
            spatial_retirement_receiver,
            command_sender,
            engine: Mutex::new(Some(engine)),
        })
    }

    fn validate_config(config: &crate::config::PetalSonicWorldDesc) -> Result<()> {
        for (field, value) in [
            ("sample_rate", config.sample_rate as usize),
            ("block_size", config.block_size),
            ("max_emitters", config.max_emitters),
            ("max_voices", config.max_voices),
            ("control_queue_capacity", config.control_queue_capacity),
            ("event_queue_capacity", config.event_queue_capacity),
            ("timing_queue_capacity", config.timing_queue_capacity),
        ] {
            if value == 0 {
                return Err(PetalSonicError::InvalidConfiguration {
                    field,
                    reason: "must be greater than zero".into(),
                });
            }
        }
        Ok(())
    }

    pub fn config(&self) -> &crate::config::PetalSonicWorldDesc {
        &self.desc
    }

    pub fn available_output_devices() -> Result<Vec<AudioOutputDeviceInfo>> {
        PetalSonicEngine::available_output_devices()
    }

    pub fn create_emitter(&self, clip: ResidentClip, desc: EmitterDesc) -> Result<Emitter> {
        self.ensure_open()?;
        let clip = self.prepare_clip(clip)?;
        self.emitters
            .lock()
            .map_err(|_| PetalSonicError::Engine("Emitter registry is poisoned".into()))?
            .insert(EmitterState { clip, desc })
    }

    fn prepare_clip(&self, clip: ResidentClip) -> Result<ResidentClip> {
        if clip.sample_rate() == self.desc.sample_rate {
            return Ok(clip);
        }
        Ok(ResidentClip::from_audio_data(Arc::new(
            clip.data.resample(self.desc.sample_rate)?,
        )))
    }

    pub fn update_emitter(&self, emitter: Emitter, desc: EmitterDesc) -> Result<()> {
        let mut emitters = self
            .emitters
            .lock()
            .map_err(|_| PetalSonicError::Engine("Emitter registry is poisoned".into()))?;
        let state = emitters.get_mut(emitter)?;
        if state.desc.is_spatial() != desc.is_spatial() {
            return Err(PetalSonicError::InvalidConfiguration {
                field: "emitter_desc",
                reason: "spatial placement cannot change after emitter creation".into(),
            });
        }
        self.try_send(PlaybackCommand::UpdateEmitter(
            emitter,
            desc.source_config(0.0),
        ))?;
        state.desc = desc;
        Ok(())
    }

    /// Publishes the latest complete listener + spatial-emitter transform set.
    ///
    /// An unconsumed older frame is replaced on the caller thread. The render thread
    /// observes only complete frame generations and never accumulates stale movement.
    pub fn publish_spatial_frame(&self, frame: SpatialFrame) -> Result<()> {
        self.drain_retired_spatial_frames();
        let mut latest = self
            .latest_spatial_frame
            .try_lock()
            .map_err(|_| PetalSonicError::QueuePressure)?;
        let mut emitters = self
            .emitters
            .try_lock()
            .map_err(|_| PetalSonicError::QueuePressure)?;
        emitters.apply_spatial_frame(&frame)?;
        *latest = Some(Arc::new(frame));
        Ok(())
    }

    pub fn update_direct_path_override(
        &self,
        emitter: Emitter,
        direct_path_override: Option<DirectPathOverride>,
    ) -> Result<()> {
        self.emitter_state(emitter)?;
        self.try_send(PlaybackCommand::UpdateDirectPathOverride(
            emitter,
            direct_path_override,
        ))
    }

    pub fn destroy_emitter(&self, emitter: Emitter) -> Result<()> {
        let mut emitters = self
            .emitters
            .lock()
            .map_err(|_| PetalSonicError::Engine("Emitter registry is poisoned".into()))?;
        emitters.get(emitter)?;
        self.try_send(PlaybackCommand::DestroyEmitter(emitter))?;
        emitters.remove(emitter)?;
        if let Ok(mut controlled) = self.controlled_voices.lock() {
            controlled.retain(|_, owner| *owner != emitter);
        }
        Ok(())
    }

    pub fn play(&self, emitter: Emitter, options: PlayOptions) -> Result<()> {
        self.submit_play(emitter, options, None).map(|_| ())
    }

    pub fn play_controlled(
        &self,
        emitter: Emitter,
        options: PlayOptions,
        tag: PlaybackTag,
    ) -> Result<PlaybackControl> {
        let voice_id = self.submit_play(emitter, options, Some(tag))?;
        Ok(PlaybackControl { voice_id })
    }

    fn submit_play(
        &self,
        emitter: Emitter,
        options: PlayOptions,
        completion_tag: Option<PlaybackTag>,
    ) -> Result<SourceId> {
        self.drain_retired_controls();
        self.ensure_open()?;
        let state = self.emitter_state(emitter)?;
        self.reserve_voice()?;
        let voice_id = SourceId(self.next_voice_id.fetch_add(1, Ordering::Relaxed));
        if completion_tag.is_some() {
            let mut controlled = match self.controlled_voices.lock() {
                Ok(controlled) => controlled,
                Err(_) => {
                    self.active_voice_count.fetch_sub(1, Ordering::AcqRel);
                    return Err(PetalSonicError::Engine(
                        "Playback registry is poisoned".into(),
                    ));
                }
            };
            controlled.insert(voice_id, emitter);
        }
        let command = PlaybackCommand::Play {
            voice_id,
            emitter,
            source: state.clip.data.clone(),
            config: state.desc.source_config(options.gain_db),
            loop_mode: options.loop_mode,
            detached: options.detached,
            completion_tag,
        };
        if let Err(error) = self.try_send(command) {
            self.active_voice_count.fetch_sub(1, Ordering::AcqRel);
            if completion_tag.is_some()
                && let Ok(mut controlled) = self.controlled_voices.lock()
            {
                controlled.remove(&voice_id);
            }
            return Err(error);
        }
        Ok(voice_id)
    }

    fn reserve_voice(&self) -> Result<()> {
        self.active_voice_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.desc.max_voices).then_some(active + 1)
            })
            .map(|_| ())
            .map_err(|_| PetalSonicError::CapacityExceeded {
                resource: "voice",
                limit: self.desc.max_voices,
            })
    }

    pub fn pause_emitter(&self, emitter: Emitter) -> Result<()> {
        self.emitter_state(emitter)?;
        self.try_send(PlaybackCommand::PauseEmitter(emitter))
    }

    pub fn stop_emitter(&self, emitter: Emitter) -> Result<()> {
        self.emitter_state(emitter)?;
        self.try_send(PlaybackCommand::StopEmitter(emitter))?;
        if let Ok(mut controlled) = self.controlled_voices.lock() {
            controlled.retain(|_, owner| *owner != emitter);
        }
        Ok(())
    }

    pub fn seek_emitter(&self, emitter: Emitter, progress: f32) -> Result<()> {
        self.emitter_state(emitter)?;
        self.try_send(PlaybackCommand::SeekEmitter(emitter, progress))
    }

    pub fn pause_playback(&self, control: PlaybackControl) -> Result<()> {
        self.ensure_controlled(control)?;
        self.try_send(PlaybackCommand::PauseVoice(control.voice_id))
    }

    pub fn stop_playback(&self, control: PlaybackControl) -> Result<()> {
        self.ensure_controlled(control)?;
        self.try_send(PlaybackCommand::StopVoice(control.voice_id))?;
        self.controlled_voices
            .lock()
            .map_err(|_| PetalSonicError::Engine("Playback registry is poisoned".into()))?
            .remove(&control.voice_id);
        Ok(())
    }

    pub fn seek_playback(&self, control: PlaybackControl, progress: f32) -> Result<()> {
        self.ensure_controlled(control)?;
        self.try_send(PlaybackCommand::SeekVoice(control.voice_id, progress))
    }

    pub fn stop_all(&self) -> Result<()> {
        self.try_send(PlaybackCommand::StopAll)?;
        self.controlled_voices
            .lock()
            .map_err(|_| PetalSonicError::Engine("Playback registry is poisoned".into()))?
            .clear();
        Ok(())
    }

    fn emitter_state(&self, emitter: Emitter) -> Result<EmitterState> {
        self.emitters
            .lock()
            .map_err(|_| PetalSonicError::Engine("Emitter registry is poisoned".into()))?
            .get(emitter)
            .cloned()
    }

    fn ensure_controlled(&self, control: PlaybackControl) -> Result<()> {
        self.drain_retired_controls();
        if self
            .controlled_voices
            .lock()
            .map_err(|_| PetalSonicError::Engine("Playback registry is poisoned".into()))?
            .contains_key(&control.voice_id)
        {
            Ok(())
        } else {
            Err(PetalSonicError::StalePlayback)
        }
    }

    fn try_send(&self, command: PlaybackCommand) -> Result<()> {
        match self.command_sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(PetalSonicError::QueuePressure),
            Err(TrySendError::Disconnected(_)) => Err(PetalSonicError::RuntimeClosed),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.desc.sample_rate
    }

    pub fn is_running(&self) -> bool {
        self.engine
            .lock()
            .ok()
            .and_then(|engine| engine.as_ref().map(PetalSonicEngine::is_running))
            .unwrap_or(false)
    }

    pub fn active_voice_count(&self) -> usize {
        self.active_voice_count.load(Ordering::Acquire)
    }

    pub fn frames_processed(&self) -> usize {
        self.engine
            .lock()
            .ok()
            .and_then(|engine| engine.as_ref().map(PetalSonicEngine::frames_processed))
            .unwrap_or(0)
    }

    pub fn underrun_count(&self) -> usize {
        self.engine
            .lock()
            .ok()
            .and_then(|engine| engine.as_ref().map(PetalSonicEngine::underrun_count))
            .unwrap_or(0)
    }

    pub fn drain_events(&self) -> Vec<PetalSonicEvent> {
        let events = self
            .engine
            .lock()
            .ok()
            .and_then(|engine| engine.as_ref().map(PetalSonicEngine::poll_events))
            .unwrap_or_default();
        self.drain_retired_controls();
        if let Ok(mut controlled) = self.controlled_voices.lock() {
            for event in &events {
                let PetalSonicEvent::PlaybackCompleted { control, .. } = event;
                controlled.remove(&control.voice_id);
            }
        }
        events
    }

    fn drain_retired_controls(&self) {
        if let Ok(mut controlled) = self.controlled_voices.lock() {
            while let Ok(voice_id) = self.retirement_receiver.try_recv() {
                controlled.remove(&voice_id);
            }
        }
    }

    fn drain_retired_spatial_frames(&self) {
        while self.spatial_retirement_receiver.try_recv().is_ok() {}
    }

    pub fn drain_timing_events(&self) -> Vec<RenderTimingEvent> {
        self.engine
            .lock()
            .ok()
            .and_then(|engine| engine.as_ref().map(PetalSonicEngine::poll_timing_events))
            .unwrap_or_default()
    }

    pub fn close(&self) -> Result<()> {
        let mut engine = self
            .engine
            .lock()
            .map_err(|_| PetalSonicError::Engine("Audio runtime lock is poisoned".into()))?;
        if let Some(mut engine) = engine.take() {
            engine.stop()?;
            self.active_voice_count.store(0, Ordering::Release);
        }
        Ok(())
    }

    fn ensure_open(&self) -> Result<()> {
        if self
            .engine
            .lock()
            .map_err(|_| PetalSonicError::Engine("Audio runtime lock is poisoned".into()))?
            .is_some()
        {
            Ok(())
        } else {
            Err(PetalSonicError::RuntimeClosed)
        }
    }
}

impl Drop for PetalSonicWorld {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_data::PetalSonicAudioData;
    use std::time::Duration;

    fn clip() -> ResidentClip {
        ResidentClip::from_audio_data(Arc::new(PetalSonicAudioData::new(
            vec![0.0; 16],
            48_000,
            1,
            Duration::from_secs_f64(16.0 / 48_000.0),
        )))
    }

    #[test]
    fn emitter_registry_rejects_stale_generation_after_slot_reuse() {
        let mut registry = EmitterRegistry::new(1);
        let first = registry
            .insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::default(),
            })
            .unwrap();
        registry.remove(first).unwrap();
        let second = registry
            .insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::default(),
            })
            .unwrap();

        assert_eq!(first.index, second.index);
        assert_ne!(first.generation, second.generation);
        assert!(matches!(
            registry.get(first),
            Err(PetalSonicError::StaleEmitter)
        ));
    }

    #[test]
    fn emitter_registry_enforces_capacity_and_recovers_after_remove() {
        let mut registry = EmitterRegistry::new(1);
        let emitter = registry
            .insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::default(),
            })
            .unwrap();
        assert!(matches!(
            registry.insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::default(),
            }),
            Err(PetalSonicError::CapacityExceeded {
                resource: "emitter",
                limit: 1
            })
        ));
        registry.remove(emitter).unwrap();
        registry
            .insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::default(),
            })
            .unwrap();
    }

    #[test]
    fn spatial_frame_must_be_complete_before_any_pose_is_updated() {
        let mut registry = EmitterRegistry::new(2);
        let first = registry
            .insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::spatial(Pose::default()),
            })
            .unwrap();
        let second = registry
            .insert(EmitterState {
                clip: clip(),
                desc: EmitterDesc::spatial(Pose::default()),
            })
            .unwrap();
        let moved = Pose::from_position(crate::math::Vec3::new(1.0, 2.0, 3.0));

        let incomplete = SpatialFrame::new(
            Pose::default(),
            vec![crate::domain::EmitterSpatialState::new(first, moved)],
        );
        assert!(matches!(
            registry.apply_spatial_frame(&incomplete),
            Err(PetalSonicError::InvalidConfiguration {
                field: "spatial_frame",
                ..
            })
        ));
        assert_eq!(
            registry.get(first).unwrap().desc.pose(),
            Some(Pose::default())
        );

        let complete = SpatialFrame::new(
            Pose::default(),
            vec![
                crate::domain::EmitterSpatialState::new(first, moved),
                crate::domain::EmitterSpatialState::new(second, Pose::default()),
            ],
        );
        registry.apply_spatial_frame(&complete).unwrap();
        assert_eq!(registry.get(first).unwrap().desc.pose(), Some(moved));
    }
}
