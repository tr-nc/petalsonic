use crate::acoustics::{AcousticMaterial, AcousticRay, AcousticSceneSnapshot};
use crate::domain::{
    DirectGeometry, DirectPath, DirectPlacement, Emitter, EnvironmentOrigin, EnvironmentSend,
    SpatialFrame,
};
use crate::math::{Pose, Vec3};
use crate::spatial::LateReverbParameters;
use crate::world::SourceId;
use std::cmp::Ordering as CmpOrdering;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const SPEED_OF_SOUND_METERS_PER_SECOND: f32 = 343.0;
const SOLVE_INTERVAL: Duration = Duration::from_millis(33);
pub(crate) const MAX_EARLY_REFLECTION_TAPS: usize = 2;
const EARLY_REFLECTION_MAX_DELAY_SECONDS: f32 = 0.25;
const EARLY_REFLECTION_GAIN: f32 = 0.6;
const MAX_TRACE_DISTANCE_METERS: f32 = 120.0;
const RAY_EPSILON_METERS: f32 = 0.05;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AcousticSolvePlan {
    max_direct_sources: usize,
    max_early_reflection_sources: usize,
    early_reflection_taps: usize,
    early_reflection_ray_count: usize,
    late_ray_count: usize,
    late_bounce_count: usize,
}

impl AcousticSolvePlan {
    fn for_quality(quality: f32) -> Self {
        let quality = quality.clamp(0.0, 1.0);
        Self {
            max_direct_sources: quality_count(32, 32, 64, quality),
            // Keep the existing bounded crossfade pool; higher quality improves path sampling
            // without imposing more persistent render-thread state on lower settings.
            max_early_reflection_sources: quality_count(4, 8, 8, quality),
            early_reflection_taps: if quality < 0.25 { 1 } else { 2 },
            early_reflection_ray_count: quality_count(32, 64, 256, quality),
            late_ray_count: quality_count(128, 256, 1_024, quality),
            late_bounce_count: quality_count(4, 8, 12, quality),
        }
    }
}

fn quality_count(low: usize, balanced: usize, high: usize, quality: f32) -> usize {
    let (start, end, progress) = if quality <= 0.5 {
        (low, balanced, quality * 2.0)
    } else {
        (balanced, high, (quality - 0.5) * 2.0)
    };
    (start as f32 + (end as f32 - start as f32) * progress).round() as usize
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EarlyReflectionTap {
    pub path_id: u16,
    pub arrival_direction: Vec3,
    pub delay_seconds: f32,
    pub gain: [f32; 3],
}

#[derive(Clone, Debug)]
pub(crate) struct DirectAcousticResponse {
    pub voice_id: SourceId,
    pub gain: [f32; 3],
    pub environment_gain: [f32; 3],
    pub early_reflections: Vec<EarlyReflectionTap>,
}

#[derive(Debug)]
pub(crate) struct AcousticResponse {
    pub spatial_revision: u64,
    pub geometry_version: u64,
    pub direct: Vec<DirectAcousticResponse>,
    pub late_reverb: LateReverbParameters,
    pub published_at: Instant,
    pub solve_time_us: u64,
}

impl AcousticResponse {
    pub(crate) fn direct_gain(&self, voice_id: SourceId) -> [f32; 3] {
        self.direct
            .iter()
            .find(|response| response.voice_id == voice_id)
            .map(|response| response.gain)
            .unwrap_or([1.0; 3])
    }

    pub(crate) fn environment_gain(&self, voice_id: SourceId) -> [f32; 3] {
        self.direct
            .iter()
            .find(|response| response.voice_id == voice_id)
            .map(|response| response.environment_gain)
            .unwrap_or([1.0; 3])
    }

    pub(crate) fn early_reflections(&self, voice_id: SourceId) -> &[EarlyReflectionTap] {
        self.direct
            .iter()
            .find(|response| response.voice_id == voice_id)
            .map(|response| response.early_reflections.as_slice())
            .unwrap_or_default()
    }
}

/// Immutable routing and the latest compatible emitter state for one active Voice.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AcousticVoice {
    pub voice_id: SourceId,
    pub emitter: Emitter,
    pub emitter_world_pose: Pose,
    pub acoustic_priority: f32,
    pub detached: bool,
    pub direct_path: DirectPath,
    pub environment_send: EnvironmentSend,
}

#[derive(Clone)]
struct SolveInput {
    generation: u64,
    spatial: Arc<SpatialFrame>,
    scene: Arc<AcousticSceneSnapshot>,
    voices: Vec<AcousticVoice>,
    environmental_acoustics_quality: f32,
}

struct InputState {
    generation: u64,
    spatial: Option<Arc<SpatialFrame>>,
    scene: Option<Arc<AcousticSceneSnapshot>>,
    voices: Vec<AcousticVoice>,
    environmental_acoustics_quality: f32,
}

impl InputState {
    fn new(environmental_acoustics_quality: f32, max_voices: usize) -> Self {
        Self {
            generation: 0,
            spatial: None,
            scene: None,
            voices: Vec::with_capacity(max_voices),
            environmental_acoustics_quality,
        }
    }

    fn capture(&self) -> Option<SolveInput> {
        Some(SolveInput {
            generation: self.generation,
            spatial: self.spatial.clone()?,
            scene: self.scene.clone()?,
            voices: self.voices.clone(),
            environmental_acoustics_quality: self.environmental_acoustics_quality,
        })
    }
}

struct SharedInput {
    state: Mutex<InputState>,
    changed: Condvar,
}

/// Bounded render-runtime port for active Voice acoustics state.
#[derive(Clone)]
pub(crate) struct AcousticVoiceInput {
    input: Arc<SharedInput>,
}

impl AcousticVoiceInput {
    #[cfg(test)]
    pub(crate) fn isolated(max_voices: usize) -> Self {
        Self {
            input: Arc::new(SharedInput {
                state: Mutex::new(InputState::new(0.5, max_voices)),
                changed: Condvar::new(),
            }),
        }
    }

    pub(crate) fn activate(&self, mut voice: AcousticVoice) {
        let Ok(mut state) = self.input.state.lock() else {
            return;
        };
        if let Some(spatial) = &state.spatial
            && let Some(emitter) = spatial
                .emitters()
                .iter()
                .find(|candidate| candidate.emitter == voice.emitter)
        {
            voice.emitter_world_pose = emitter.pose;
            voice.acoustic_priority = emitter.acoustic_priority();
        }
        if let Some(current) = state
            .voices
            .iter_mut()
            .find(|current| current.voice_id == voice.voice_id)
        {
            *current = voice;
        } else if state.voices.len() < state.voices.capacity() {
            state.voices.push(voice);
        } else {
            return;
        }
        state.generation = state.generation.wrapping_add(1).max(1);
        drop(state);
        self.input.changed.notify_one();
    }

    pub(crate) fn retire(&self, voice_id: SourceId) {
        let Ok(mut state) = self.input.state.lock() else {
            return;
        };
        let Some(index) = state
            .voices
            .iter()
            .position(|voice| voice.voice_id == voice_id)
        else {
            return;
        };
        state.voices.swap_remove(index);
        state.generation = state.generation.wrapping_add(1).max(1);
        drop(state);
        self.input.changed.notify_one();
    }
}

pub(crate) struct AcousticPropagationCounters {
    solve_count: AtomicU64,
    superseded_solve_count: AtomicU64,
    published_response_count: AtomicU64,
    latest_spatial_revision: AtomicU64,
    latest_geometry_version: AtomicU64,
    last_solve_time_us: AtomicU64,
    solve_time_max_us: AtomicU64,
    solve_time_histogram: [AtomicU64; 64],
    last_publication: Mutex<Option<Instant>>,
}

impl Default for AcousticPropagationCounters {
    fn default() -> Self {
        Self {
            solve_count: AtomicU64::new(0),
            superseded_solve_count: AtomicU64::new(0),
            published_response_count: AtomicU64::new(0),
            latest_spatial_revision: AtomicU64::new(0),
            latest_geometry_version: AtomicU64::new(0),
            last_solve_time_us: AtomicU64::new(0),
            solve_time_max_us: AtomicU64::new(0),
            solve_time_histogram: std::array::from_fn(|_| AtomicU64::new(0)),
            last_publication: Mutex::new(None),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AcousticPropagationDiagnostics {
    pub solve_count: u64,
    pub superseded_solve_count: u64,
    pub published_response_count: u64,
    pub latest_spatial_revision: u64,
    pub latest_geometry_version: u64,
    pub last_solve_time_us: u64,
    pub solve_time_p50_us: u64,
    pub solve_time_p95_us: u64,
    pub solve_time_p99_us: u64,
    pub solve_time_max_us: u64,
    pub response_age_ms: u64,
}

impl AcousticPropagationCounters {
    fn record_solve(&self, elapsed_us: u64) {
        self.solve_count.fetch_add(1, Ordering::Relaxed);
        self.last_solve_time_us.store(elapsed_us, Ordering::Relaxed);
        self.solve_time_max_us
            .fetch_max(elapsed_us, Ordering::Relaxed);
        self.solve_time_histogram[histogram_bucket(elapsed_us)].fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> AcousticPropagationDiagnostics {
        let total = self
            .solve_time_histogram
            .iter()
            .map(|bucket| bucket.load(Ordering::Relaxed))
            .sum();
        let response_age_ms = self
            .last_publication
            .lock()
            .ok()
            .and_then(|published| *published)
            .map(|published| published.elapsed().as_millis() as u64)
            .unwrap_or(0);
        AcousticPropagationDiagnostics {
            solve_count: self.solve_count.load(Ordering::Relaxed),
            superseded_solve_count: self.superseded_solve_count.load(Ordering::Relaxed),
            published_response_count: self.published_response_count.load(Ordering::Relaxed),
            latest_spatial_revision: self.latest_spatial_revision.load(Ordering::Acquire),
            latest_geometry_version: self.latest_geometry_version.load(Ordering::Acquire),
            last_solve_time_us: self.last_solve_time_us.load(Ordering::Relaxed),
            solve_time_p50_us: histogram_percentile(&self.solve_time_histogram, total, 50),
            solve_time_p95_us: histogram_percentile(&self.solve_time_histogram, total, 95),
            solve_time_p99_us: histogram_percentile(&self.solve_time_histogram, total, 99),
            solve_time_max_us: self.solve_time_max_us.load(Ordering::Relaxed),
            response_age_ms,
        }
    }
}

pub(crate) struct AcousticPropagation {
    input: Arc<SharedInput>,
    latest_response: Arc<Mutex<Option<Arc<AcousticResponse>>>>,
    counters: Arc<AcousticPropagationCounters>,
    enabled: Arc<AtomicBool>,
    quality_bits: AtomicU32,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl AcousticPropagation {
    pub(crate) fn new(
        distance_scaler: f32,
        enabled: Arc<AtomicBool>,
        environmental_acoustics_quality: f32,
        max_voices: usize,
    ) -> std::io::Result<Self> {
        let input = Arc::new(SharedInput {
            state: Mutex::new(InputState::new(environmental_acoustics_quality, max_voices)),
            changed: Condvar::new(),
        });
        let latest_response = Arc::new(Mutex::new(None));
        let counters = Arc::new(AcousticPropagationCounters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let input = input.clone();
            let latest_response = latest_response.clone();
            let counters = counters.clone();
            let enabled = enabled.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("petalsonic-acoustics".into())
                .spawn(move || {
                    propagation_loop(
                        &input,
                        &latest_response,
                        &counters,
                        &enabled,
                        &stop,
                        distance_scaler,
                    )
                })?
        };
        Ok(Self {
            input,
            latest_response,
            counters,
            enabled,
            quality_bits: AtomicU32::new(environmental_acoustics_quality.to_bits()),
            stop,
            worker: Mutex::new(Some(worker)),
        })
    }

    pub(crate) fn publish_spatial_frame(
        &self,
        frame: Arc<SpatialFrame>,
    ) -> std::result::Result<(), Arc<SpatialFrame>> {
        let Ok(mut state) = self.input.state.lock() else {
            return Err(frame);
        };
        state.generation = state.generation.wrapping_add(1).max(1);
        for voice in &mut state.voices {
            if voice.detached {
                continue;
            }
            if let Some(emitter) = frame
                .emitters()
                .iter()
                .find(|candidate| candidate.emitter == voice.emitter)
            {
                voice.emitter_world_pose = emitter.pose;
                voice.acoustic_priority = emitter.acoustic_priority();
            }
        }
        state.spatial = Some(frame);
        drop(state);
        self.input.changed.notify_one();
        Ok(())
    }

    pub(crate) fn publish_scene(
        &self,
        scene: Arc<AcousticSceneSnapshot>,
    ) -> std::result::Result<(), Arc<AcousticSceneSnapshot>> {
        let Ok(mut state) = self.input.state.lock() else {
            return Err(scene);
        };
        state.generation = state.generation.wrapping_add(1).max(1);
        state.scene = Some(scene);
        drop(state);
        self.input.changed.notify_one();
        Ok(())
    }

    pub(crate) fn latest_response_slot(&self) -> Arc<Mutex<Option<Arc<AcousticResponse>>>> {
        self.latest_response.clone()
    }

    pub(crate) fn voice_input(&self) -> AcousticVoiceInput {
        AcousticVoiceInput {
            input: self.input.clone(),
        }
    }

    pub(crate) fn diagnostics(&self) -> AcousticPropagationDiagnostics {
        self.counters.snapshot()
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        if self.enabled.load(Ordering::Acquire) == enabled {
            return;
        }
        // Change the predicate while holding the waiter's mutex so this notification cannot land
        // between the worker's predicate check and Condvar::wait.
        let state = self.input.state.lock().ok();
        self.enabled.store(enabled, Ordering::Release);
        drop(state);
        self.input.changed.notify_one();
    }

    pub(crate) fn set_quality(&self, quality: f32) {
        let quality_bits = quality.to_bits();
        if self.quality_bits.load(Ordering::Acquire) == quality_bits {
            return;
        }
        let Ok(mut state) = self.input.state.lock() else {
            return;
        };
        if state.environmental_acoustics_quality.to_bits() == quality_bits {
            self.quality_bits.store(quality_bits, Ordering::Release);
            return;
        }
        state.environmental_acoustics_quality = quality;
        state.generation = state.generation.wrapping_add(1).max(1);
        self.quality_bits.store(quality_bits, Ordering::Release);
        drop(state);
        self.input.changed.notify_one();
    }

    pub(crate) fn quality(&self) -> f32 {
        f32::from_bits(self.quality_bits.load(Ordering::Acquire))
    }

    pub(crate) fn close(&self) {
        let state = self.input.state.lock().ok();
        self.stop.store(true, Ordering::Release);
        drop(state);
        self.input.changed.notify_one();
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
        if let Ok(mut response) = self.latest_response.lock() {
            response.take();
        }
    }
}

impl Drop for AcousticPropagation {
    fn drop(&mut self) {
        self.close();
    }
}

fn propagation_loop(
    input: &SharedInput,
    latest_response: &Mutex<Option<Arc<AcousticResponse>>>,
    counters: &AcousticPropagationCounters,
    enabled: &AtomicBool,
    stop: &AtomicBool,
    distance_scaler: f32,
) {
    let mut consumed_generation = 0;
    let mut next_solve = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let captured = {
            let Ok(mut state) = input.state.lock() else {
                return;
            };
            loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                if !enabled.load(Ordering::Acquire) {
                    let Ok(next_state) = input.changed.wait(state) else {
                        return;
                    };
                    state = next_state;
                    continue;
                }
                let captured = (state.generation != consumed_generation)
                    .then(|| state.capture())
                    .flatten();
                let now = Instant::now();
                if captured.is_some() && now >= next_solve {
                    break captured;
                }
                if captured.is_none() {
                    let Ok(next_state) = input.changed.wait(state) else {
                        return;
                    };
                    state = next_state;
                } else {
                    let wait = next_solve.saturating_duration_since(now);
                    let Ok((next_state, _)) = input.changed.wait_timeout(state, wait) else {
                        return;
                    };
                    state = next_state;
                }
            }
        };
        let Some(captured) = captured else {
            continue;
        };
        consumed_generation = captured.generation;
        next_solve = Instant::now() + SOLVE_INTERVAL;

        let started = Instant::now();
        let mut response = solve_response(&captured, distance_scaler);
        let elapsed_us = started.elapsed().as_micros() as u64;
        response.solve_time_us = elapsed_us;
        counters.record_solve(elapsed_us);

        let superseded = input
            .state
            .lock()
            .map(|state| state.generation != captured.generation)
            .unwrap_or(true);
        if superseded {
            counters
                .superseded_solve_count
                .fetch_add(1, Ordering::Relaxed);
            continue;
        }

        response.published_at = Instant::now();
        counters
            .latest_spatial_revision
            .store(response.spatial_revision, Ordering::Release);
        counters
            .latest_geometry_version
            .store(response.geometry_version, Ordering::Release);
        counters
            .published_response_count
            .fetch_add(1, Ordering::Relaxed);
        if let Ok(mut publication) = counters.last_publication.lock() {
            *publication = Some(response.published_at);
        }
        if let Ok(mut latest) = latest_response.lock() {
            *latest = Some(Arc::new(response));
        }
    }
}

fn solve_response(input: &SolveInput, distance_scaler: f32) -> AcousticResponse {
    let plan = AcousticSolvePlan::for_quality(input.environmental_acoustics_quality);
    let candidates = ranked_voices(input, plan);
    let mut direct = solve_direct(input, &candidates, distance_scaler);
    solve_early_reflections(input, &candidates, distance_scaler, plan, &mut direct);
    AcousticResponse {
        spatial_revision: input.spatial.revision(),
        geometry_version: input.scene.version(),
        direct,
        late_reverb: solve_late_reverb(input, distance_scaler, plan),
        published_at: Instant::now(),
        solve_time_us: 0,
    }
}

#[derive(Clone, Copy, Debug)]
struct RankedVoice {
    voice: AcousticVoice,
    direct_origin: Option<Vec3>,
    environment_origin: Option<Vec3>,
}

fn ranked_voices(input: &SolveInput, plan: AcousticSolvePlan) -> Vec<RankedVoice> {
    let listener_pose = input.spatial.listener();
    let listener = listener_pose.position;
    let mut candidates: Vec<(f32, RankedVoice)> = input
        .voices
        .iter()
        .filter_map(|voice| {
            let direct_origin = match voice.direct_path.placement() {
                DirectPlacement::World
                    if matches!(
                        voice.direct_path.geometry(),
                        DirectGeometry::SimulatedTransmission
                    ) =>
                {
                    Some(voice.emitter_world_pose.position)
                }
                DirectPlacement::ListenerRelative(local_pose)
                    if matches!(
                        voice.direct_path.geometry(),
                        DirectGeometry::SimulatedTransmission
                    ) =>
                {
                    Some(listener_to_world_position(
                        listener_pose,
                        local_pose.position,
                    ))
                }
                _ => None,
            };
            let environment_origin = match voice.environment_send.origin() {
                EnvironmentOrigin::FollowEmitter => Some(voice.emitter_world_pose.position),
                EnvironmentOrigin::World(origin) => Some(origin.position),
                EnvironmentOrigin::Disabled => None,
            };
            let origin = environment_origin.or(direct_origin)?;
            let distance = origin.distance(listener);
            let priority = voice.acoustic_priority;
            (distance.is_finite() && priority.is_finite() && priority > 0.0).then_some((
                priority / (1.0 + distance),
                RankedVoice {
                    voice: *voice,
                    direct_origin,
                    environment_origin,
                },
            ))
        })
        .collect();
    candidates.sort_by(|left, right| right.0.partial_cmp(&left.0).unwrap_or(CmpOrdering::Equal));
    candidates.truncate(plan.max_direct_sources);
    candidates.into_iter().map(|(_, voice)| voice).collect()
}

fn solve_direct(
    input: &SolveInput,
    candidates: &[RankedVoice],
    distance_scaler: f32,
) -> Vec<DirectAcousticResponse> {
    let listener = input.spatial.listener().position;
    let ray_epsilon_world = RAY_EPSILON_METERS / distance_scaler.max(0.001);

    let direct_gain = trace_transmission(
        input,
        listener,
        ray_epsilon_world,
        candidates.iter().map(|voice| voice.direct_origin),
    );
    let environment_gain = trace_transmission(
        input,
        listener,
        ray_epsilon_world,
        candidates.iter().map(|voice| voice.environment_origin),
    );

    candidates
        .iter()
        .zip(direct_gain)
        .zip(environment_gain)
        .map(
            |((candidate, gain), environment_gain)| DirectAcousticResponse {
                voice_id: candidate.voice.voice_id,
                gain,
                environment_gain,
                early_reflections: Vec::with_capacity(MAX_EARLY_REFLECTION_TAPS),
            },
        )
        .collect()
}

fn trace_transmission(
    input: &SolveInput,
    listener: Vec3,
    ray_epsilon_world: f32,
    origins: impl Iterator<Item = Option<Vec3>>,
) -> Vec<[f32; 3]> {
    let origins: Vec<_> = origins.collect();
    let mut rays = Vec::with_capacity(origins.len());
    let mut min_distances = Vec::with_capacity(origins.len());
    let mut max_distances = Vec::with_capacity(origins.len());
    for origin in &origins {
        let delta = origin.unwrap_or(listener) - listener;
        let distance = delta.length();
        rays.push(AcousticRay {
            origin: listener,
            direction: normalize_or(delta, Vec3::Z),
        });
        let max_distance = (distance - ray_epsilon_world).max(0.0);
        min_distances.push(ray_epsilon_world.min(max_distance));
        max_distances.push(max_distance);
    }
    let mut hits = vec![None; rays.len()];
    input
        .scene
        .query()
        .trace_closest_hit_batch(&rays, &min_distances, &max_distances, &mut hits);

    origins
        .into_iter()
        .zip(hits)
        .zip(min_distances.into_iter().zip(max_distances))
        .map(|((origin, hit), (min_distance, max_distance))| {
            origin
                .map(|_| {
                    hit.filter(|hit| valid_hit_distance(hit.distance, min_distance, max_distance))
                        .map(|hit| {
                            hit.material
                                .transmission
                                .map(|gain| sanitize_unit(gain, 1.0))
                        })
                        .unwrap_or([1.0; 3])
                })
                .unwrap_or([1.0; 3])
        })
        .collect()
}

fn solve_early_reflections(
    input: &SolveInput,
    candidates: &[RankedVoice],
    distance_scaler: f32,
    plan: AcousticSolvePlan,
    responses: &mut [DirectAcousticResponse],
) {
    let listener = input.spatial.listener().position;
    let ray_epsilon_world = RAY_EPSILON_METERS / distance_scaler.max(0.001);
    let max_distance_world = MAX_TRACE_DISTANCE_METERS / distance_scaler.max(0.001);
    let probe_rays: Vec<_> = (0..plan.early_reflection_ray_count)
        .map(|index| AcousticRay {
            origin: listener,
            direction: fibonacci_direction(index, plan.early_reflection_ray_count),
        })
        .collect();
    let min_distances = vec![ray_epsilon_world; plan.early_reflection_ray_count];
    let max_distances = vec![max_distance_world; plan.early_reflection_ray_count];
    let mut surface_hits = vec![None; plan.early_reflection_ray_count];
    input.scene.query().trace_closest_hit_batch(
        &probe_rays,
        &min_distances,
        &max_distances,
        &mut surface_hits,
    );

    for candidate in candidates.iter().take(plan.max_early_reflection_sources) {
        let Some(source_position) = candidate.environment_origin else {
            continue;
        };
        let direct_distance_world = source_position.distance(listener);
        if !direct_distance_world.is_finite() {
            continue;
        }
        let mut visibility_rays = Vec::with_capacity(plan.early_reflection_ray_count);
        let mut visibility_min = Vec::with_capacity(plan.early_reflection_ray_count);
        let mut visibility_max = Vec::with_capacity(plan.early_reflection_ray_count);
        let mut candidates_for_visibility = Vec::with_capacity(plan.early_reflection_ray_count);

        for (path_id, (probe_ray, hit)) in probe_rays.iter().zip(&surface_hits).enumerate() {
            let Some(hit) = hit.filter(|hit| {
                valid_hit_distance(hit.distance, ray_epsilon_world, max_distance_world)
            }) else {
                continue;
            };
            let hit_position = listener + probe_ray.direction * hit.distance;
            let hit_to_source = source_position - hit_position;
            let hit_to_source_distance = hit_to_source.length();
            if !hit_to_source_distance.is_finite()
                || hit_to_source_distance <= ray_epsilon_world * 2.0
            {
                continue;
            }
            let to_source = hit_to_source / hit_to_source_distance;
            let incoming = -to_source;
            let mut normal = normalize_or(hit.normal, -incoming);
            if normal.dot(incoming) > 0.0 {
                normal = -normal;
            }
            let to_listener = -probe_ray.direction;
            let reflected =
                normalize_or(incoming - 2.0 * incoming.dot(normal) * normal, to_listener);
            let specular_alignment = reflected.dot(to_listener).max(0.0).powi(8);
            let cosine_in = normal.dot(-incoming).max(0.0);
            let cosine_out = normal.dot(to_listener).max(0.0);
            let scattering = sanitize_unit(
                hit.material.scattering,
                AcousticMaterial::default().scattering,
            );
            let path_weight = (specular_alignment * (1.0 - scattering)
                + cosine_in * cosine_out * scattering * 0.25)
                .clamp(0.0, 1.0);
            if path_weight <= 1.0e-4 {
                continue;
            }
            let total_distance_world = hit.distance + hit_to_source_distance;
            let excess_distance_meters =
                (total_distance_world - direct_distance_world).max(0.0) * distance_scaler;
            let delay_seconds = excess_distance_meters / SPEED_OF_SOUND_METERS_PER_SECOND;
            if !delay_seconds.is_finite()
                || !(f32::EPSILON..=EARLY_REFLECTION_MAX_DELAY_SECONDS).contains(&delay_seconds)
            {
                continue;
            }
            let total_distance_meters = total_distance_world * distance_scaler;
            let propagation_gain = EARLY_REFLECTION_GAIN
                * path_weight
                * propagation_air_absorption(total_distance_meters)
                * propagation_distance_attenuation(total_distance_meters);
            let gain = std::array::from_fn(|band| {
                propagation_gain
                    * (1.0
                        - sanitize_unit(
                            hit.material.absorption[band],
                            AcousticMaterial::default().absorption[band],
                        ))
            });
            if gain.iter().all(|gain| *gain <= 1.0e-5) {
                continue;
            }

            visibility_rays.push(AcousticRay {
                origin: hit_position + to_source * ray_epsilon_world,
                direction: to_source,
            });
            visibility_min.push(0.0);
            visibility_max.push((hit_to_source_distance - ray_epsilon_world * 2.0).max(0.0));
            candidates_for_visibility.push(EarlyReflectionTap {
                path_id: path_id as u16,
                arrival_direction: probe_ray.direction,
                delay_seconds,
                gain,
            });
        }

        if visibility_rays.is_empty() {
            continue;
        }
        let mut blocked = vec![false; visibility_rays.len()];
        input.scene.query().trace_any_hit_batch(
            &visibility_rays,
            &visibility_min,
            &visibility_max,
            &mut blocked,
        );
        let mut taps: Vec<_> = candidates_for_visibility
            .into_iter()
            .zip(blocked)
            .filter_map(|(tap, blocked)| (!blocked).then_some(tap))
            .collect();
        taps.sort_by(|left, right| {
            reflection_strength(right)
                .partial_cmp(&reflection_strength(left))
                .unwrap_or(CmpOrdering::Equal)
        });
        taps.truncate(plan.early_reflection_taps);
        taps.sort_by_key(|tap| tap.path_id);
        if let Some(response) = responses
            .iter_mut()
            .find(|response| response.voice_id == candidate.voice.voice_id)
        {
            response.early_reflections = taps;
        }
    }
}

fn listener_to_world_position(listener: Pose, local_position: Vec3) -> Vec3 {
    listener.position
        + listener.right() * local_position.x
        + listener.up() * local_position.y
        + listener.forward() * local_position.z
}

fn reflection_strength(tap: &EarlyReflectionTap) -> f32 {
    tap.gain.iter().sum()
}

fn propagation_distance_attenuation(distance_meters: f32) -> f32 {
    1.0 / distance_meters.max(1.0)
}

fn propagation_air_absorption(distance_meters: f32) -> f32 {
    (-0.0002 * distance_meters.max(0.0)).exp().clamp(0.2, 1.0)
}

fn solve_late_reverb(
    input: &SolveInput,
    distance_scaler: f32,
    plan: AcousticSolvePlan,
) -> LateReverbParameters {
    let listener = input.spatial.listener().position;
    let max_distance_world = MAX_TRACE_DISTANCE_METERS / distance_scaler.max(0.001);
    let ray_epsilon_world = RAY_EPSILON_METERS / distance_scaler.max(0.001);
    let mut rays: Vec<AcousticRay> = (0..plan.late_ray_count)
        .map(|index| AcousticRay {
            origin: listener,
            direction: fibonacci_direction(index, plan.late_ray_count),
        })
        .collect();
    let min_distances = vec![ray_epsilon_world; plan.late_ray_count];
    let max_distances = vec![max_distance_world; plan.late_ray_count];
    let mut hits = vec![None; plan.late_ray_count];
    let mut active = vec![true; plan.late_ray_count];
    let mut energy = vec![[1.0f32; 3]; plan.late_ray_count];
    let mut hit_segments = 0usize;
    let mut first_bounce_hits = 0usize;
    let mut minimum_hit_distance_meters = f32::INFINITY;
    let mut segment_time_sum = 0.0f32;
    let mut log_reflectivity_sum = [0.0f32; 3];
    let mut reflected_energy_sum = [0.0f32; 3];

    for bounce in 0..plan.late_bounce_count {
        hits.fill(None);
        input.scene.query().trace_closest_hit_batch(
            &rays,
            &min_distances,
            &max_distances,
            &mut hits,
        );
        for index in 0..plan.late_ray_count {
            if !active[index] {
                continue;
            }
            let Some(hit) = hits[index].filter(|hit| {
                valid_hit_distance(hit.distance, ray_epsilon_world, max_distance_world)
            }) else {
                active[index] = false;
                continue;
            };
            if bounce == 0 {
                first_bounce_hits += 1;
                minimum_hit_distance_meters =
                    minimum_hit_distance_meters.min(hit.distance.max(0.0) * distance_scaler);
            }
            hit_segments += 1;
            segment_time_sum +=
                hit.distance.max(0.0) * distance_scaler / SPEED_OF_SOUND_METERS_PER_SECOND;
            for band in 0..3 {
                let absorption = sanitize_unit(
                    hit.material.absorption[band],
                    AcousticMaterial::default().absorption[band],
                );
                let reflectivity = (1.0 - absorption).clamp(0.01, 0.999);
                log_reflectivity_sum[band] += reflectivity.ln();
                energy[index][band] *= reflectivity;
                reflected_energy_sum[band] += energy[index][band];
            }
            if energy[index].iter().all(|value| *value < 1.0e-4) {
                active[index] = false;
                continue;
            }

            let normal = normalize_or(hit.normal, -rays[index].direction);
            let specular = rays[index].direction - 2.0 * rays[index].direction.dot(normal) * normal;
            let diffuse = deterministic_hemisphere(index, bounce, normal);
            let scattering = sanitize_unit(
                hit.material.scattering,
                AcousticMaterial::default().scattering,
            );
            let direction = normalize_or(specular.lerp(diffuse, scattering), specular);
            rays[index] = AcousticRay {
                origin: rays[index].origin
                    + rays[index].direction * hit.distance
                    + normal * ray_epsilon_world,
                direction,
            };
        }
    }

    if hit_segments == 0 || first_bounce_hits == 0 {
        return LateReverbParameters::SILENT;
    }
    let mean_segment_time = segment_time_sum / hit_segments as f32;
    let rt60_seconds = std::array::from_fn(|band| {
        let mean_log_reflectivity = log_reflectivity_sum[band] / hit_segments as f32;
        if mean_log_reflectivity >= -f32::EPSILON {
            20.0
        } else {
            (-6.0 * mean_segment_time / (mean_log_reflectivity / std::f32::consts::LN_10))
                .clamp(0.05, 20.0)
        }
    });
    let enclosure = first_bounce_hits as f32 / plan.late_ray_count as f32;
    let reflected_energy =
        reflected_energy_sum.iter().sum::<f32>() / (hit_segments.max(1) * 3) as f32;
    LateReverbParameters {
        pre_delay_seconds: (minimum_hit_distance_meters * 2.0 / SPEED_OF_SOUND_METERS_PER_SECOND)
            .clamp(0.0, 0.25),
        rt60_seconds,
        wet_gain: (enclosure * reflected_energy * 0.35).clamp(0.0, 0.35),
    }
}

fn fibonacci_direction(index: usize, count: usize) -> Vec3 {
    let y = 1.0 - 2.0 * (index as f32 + 0.5) / count as f32;
    let radius = (1.0 - y * y).max(0.0).sqrt();
    let angle = std::f32::consts::TAU * index as f32 * 0.618_034;
    Vec3::new(radius * angle.cos(), y, radius * angle.sin())
}

fn deterministic_hemisphere(index: usize, bounce: usize, normal: Vec3) -> Vec3 {
    let seed = (index as u32)
        .wrapping_mul(0x9e37_79b9)
        .wrapping_add((bounce as u32).wrapping_mul(0x85eb_ca6b));
    let u = hash_unit(seed);
    let v = hash_unit(seed ^ 0xc2b2_ae35);
    let radius = u.sqrt();
    let angle = std::f32::consts::TAU * v;
    let local = Vec3::new(radius * angle.cos(), (1.0 - u).sqrt(), radius * angle.sin());
    let tangent = normalize_or(
        if normal.y.abs() < 0.9 {
            normal.cross(Vec3::Y)
        } else {
            normal.cross(Vec3::X)
        },
        Vec3::X,
    );
    let bitangent = normal.cross(tangent);
    normalize_or(
        tangent * local.x + normal * local.y + bitangent * local.z,
        normal,
    )
}

fn hash_unit(mut value: u32) -> f32 {
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    value ^= value >> 16;
    value as f32 / u32::MAX as f32
}

fn normalize_or(value: Vec3, fallback: Vec3) -> Vec3 {
    if value.is_finite() && value.length_squared() > f32::EPSILON {
        value.normalize()
    } else {
        fallback
    }
}

fn valid_hit_distance(distance: f32, min_distance: f32, max_distance: f32) -> bool {
    distance.is_finite()
        && distance >= min_distance
        && distance <= max_distance
        && max_distance > 0.0
}

fn sanitize_unit(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        fallback
    }
}

fn histogram_bucket(value: u64) -> usize {
    if value == 0 {
        0
    } else {
        ((u64::BITS - (value - 1).leading_zeros()) as usize + 1).min(63)
    }
}

fn histogram_percentile(histogram: &[AtomicU64; 64], total: u64, percentile: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    let target = total.saturating_mul(percentile).div_ceil(100);
    let mut cumulative = 0;
    for (index, bucket) in histogram.iter().enumerate() {
        cumulative += bucket.load(Ordering::Relaxed);
        if cumulative >= target {
            return match index {
                0 => 0,
                63 => u64::MAX,
                _ => 1u64 << (index - 1),
            };
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acoustics::{AcousticHit, AcousticMaterial, AcousticRayQuerySnapshot};
    use crate::domain::EmitterSpatialState;
    use crate::math::Pose;

    struct NoGeometry;

    impl AcousticRayQuerySnapshot for NoGeometry {
        fn trace_any_hit_batch(
            &self,
            _rays: &[AcousticRay],
            _min_distances: &[f32],
            _max_distances: &[f32],
            hits: &mut [bool],
        ) {
            hits.fill(false);
        }

        fn trace_closest_hit_batch(
            &self,
            _rays: &[AcousticRay],
            _min_distances: &[f32],
            _max_distances: &[f32],
            hits: &mut [Option<AcousticHit>],
        ) {
            hits.fill(None);
        }
    }

    struct UnitRoom;

    impl AcousticRayQuerySnapshot for UnitRoom {
        fn trace_any_hit_batch(
            &self,
            rays: &[AcousticRay],
            _min_distances: &[f32],
            _max_distances: &[f32],
            hits: &mut [bool],
        ) {
            hits[..rays.len()].fill(true);
        }

        fn trace_closest_hit_batch(
            &self,
            rays: &[AcousticRay],
            _min_distances: &[f32],
            max_distances: &[f32],
            hits: &mut [Option<AcousticHit>],
        ) {
            for ((ray, max_distance), hit) in rays
                .iter()
                .zip(max_distances.iter().copied())
                .zip(hits.iter_mut())
            {
                let distance = 1.0f32.min(max_distance);
                *hit = (distance > 0.0).then_some(AcousticHit {
                    distance,
                    normal: -ray.direction,
                    material: AcousticMaterial {
                        absorption: [0.2, 0.3, 0.5],
                        scattering: 0.2,
                        transmission: [0.1, 0.05, 0.02],
                    },
                });
            }
        }
    }

    struct InvalidMaterialRoom;

    impl AcousticRayQuerySnapshot for InvalidMaterialRoom {
        fn trace_any_hit_batch(
            &self,
            rays: &[AcousticRay],
            _min_distances: &[f32],
            _max_distances: &[f32],
            hits: &mut [bool],
        ) {
            hits[..rays.len()].fill(true);
        }

        fn trace_closest_hit_batch(
            &self,
            rays: &[AcousticRay],
            min_distances: &[f32],
            max_distances: &[f32],
            hits: &mut [Option<AcousticHit>],
        ) {
            for (((ray, min_distance), max_distance), hit) in rays
                .iter()
                .zip(min_distances.iter().copied())
                .zip(max_distances.iter().copied())
                .zip(hits.iter_mut())
            {
                *hit = Some(AcousticHit {
                    distance: (min_distance + max_distance) * 0.5,
                    normal: ray.direction * f32::NAN,
                    material: AcousticMaterial {
                        absorption: [f32::NAN, -1.0, 2.0],
                        scattering: f32::NAN,
                        transmission: [f32::NAN, -1.0, 2.0],
                    },
                });
            }
        }
    }

    struct DirectionalTransmission;

    impl AcousticRayQuerySnapshot for DirectionalTransmission {
        fn trace_any_hit_batch(
            &self,
            _rays: &[AcousticRay],
            _min_distances: &[f32],
            _max_distances: &[f32],
            hits: &mut [bool],
        ) {
            hits.fill(false);
        }

        fn trace_closest_hit_batch(
            &self,
            rays: &[AcousticRay],
            min_distances: &[f32],
            max_distances: &[f32],
            hits: &mut [Option<AcousticHit>],
        ) {
            for (((ray, min_distance), max_distance), hit) in rays
                .iter()
                .zip(min_distances.iter().copied())
                .zip(max_distances.iter().copied())
                .zip(hits.iter_mut())
            {
                let distance = (min_distance + max_distance) * 0.5;
                let transmission = if ray.direction.x >= 0.0 { 0.2 } else { 0.8 };
                *hit = valid_hit_distance(distance, min_distance, max_distance).then_some(
                    AcousticHit {
                        distance,
                        normal: -ray.direction,
                        material: AcousticMaterial {
                            transmission: [transmission; 3],
                            ..AcousticMaterial::default()
                        },
                    },
                );
            }
        }
    }

    struct ReflectiveFloor {
        blocks_visibility: bool,
    }

    impl AcousticRayQuerySnapshot for ReflectiveFloor {
        fn trace_any_hit_batch(
            &self,
            rays: &[AcousticRay],
            _min_distances: &[f32],
            _max_distances: &[f32],
            hits: &mut [bool],
        ) {
            hits[..rays.len()].fill(self.blocks_visibility);
        }

        fn trace_closest_hit_batch(
            &self,
            rays: &[AcousticRay],
            min_distances: &[f32],
            max_distances: &[f32],
            hits: &mut [Option<AcousticHit>],
        ) {
            for (((ray, min_distance), max_distance), hit) in rays
                .iter()
                .zip(min_distances.iter().copied())
                .zip(max_distances.iter().copied())
                .zip(hits.iter_mut())
            {
                let distance = (-1.0 - ray.origin.y) / ray.direction.y;
                *hit = (ray.direction.y < -1.0e-4
                    && valid_hit_distance(distance, min_distance, max_distance))
                .then_some(AcousticHit {
                    distance,
                    normal: Vec3::Y,
                    material: AcousticMaterial {
                        absorption: [0.1, 0.3, 0.6],
                        scattering: 0.8,
                        transmission: [0.05; 3],
                    },
                });
            }
        }
    }

    fn input(query: Arc<dyn AcousticRayQuerySnapshot>) -> SolveInput {
        let emitter = Emitter {
            world_id: 1,
            index: 0,
            generation: 1,
        };
        SolveInput {
            generation: 1,
            spatial: Arc::new(SpatialFrame::new(
                11,
                1.5,
                Pose::from_position(Vec3::ZERO),
                vec![EmitterSpatialState::new(
                    emitter,
                    Pose::from_position(Vec3::Z),
                )],
            )),
            scene: Arc::new(AcousticSceneSnapshot::new(17, query)),
            voices: vec![AcousticVoice {
                voice_id: SourceId::from(1),
                emitter,
                emitter_world_pose: Pose::from_position(Vec3::Z),
                acoustic_priority: 1.0,
                detached: false,
                direct_path: DirectPath::default(),
                environment_send: EnvironmentSend::default(),
            }],
            environmental_acoustics_quality: 0.5,
        }
    }

    #[test]
    fn quality_plans_preserve_the_existing_default_and_scale_monotonically() {
        assert_eq!(
            AcousticSolvePlan::for_quality(0.0),
            AcousticSolvePlan {
                max_direct_sources: 32,
                max_early_reflection_sources: 4,
                early_reflection_taps: 1,
                early_reflection_ray_count: 32,
                late_ray_count: 128,
                late_bounce_count: 4,
            }
        );
        assert_eq!(
            AcousticSolvePlan::for_quality(0.5),
            AcousticSolvePlan {
                max_direct_sources: 32,
                max_early_reflection_sources: 8,
                early_reflection_taps: 2,
                early_reflection_ray_count: 64,
                late_ray_count: 256,
                late_bounce_count: 8,
            }
        );
        assert_eq!(
            AcousticSolvePlan::for_quality(1.0),
            AcousticSolvePlan {
                max_direct_sources: 64,
                max_early_reflection_sources: 8,
                early_reflection_taps: 2,
                early_reflection_ray_count: 256,
                late_ray_count: 1_024,
                late_bounce_count: 12,
            }
        );

        let mut previous = AcousticSolvePlan::for_quality(0.0);
        for step in 1..=100 {
            let plan = AcousticSolvePlan::for_quality(step as f32 / 100.0);
            assert!(plan.max_direct_sources >= previous.max_direct_sources);
            assert!(plan.max_early_reflection_sources >= previous.max_early_reflection_sources);
            assert!(plan.early_reflection_taps >= previous.early_reflection_taps);
            assert!(plan.early_reflection_ray_count >= previous.early_reflection_ray_count);
            assert!(plan.late_ray_count >= previous.late_ray_count);
            assert!(plan.late_bounce_count >= previous.late_bounce_count);
            previous = plan;
        }
    }

    #[test]
    fn response_pairs_complete_spatial_and_geometry_generations() {
        let response = solve_response(&input(Arc::new(UnitRoom)), 1.0);
        assert_eq!(response.spatial_revision, 11);
        assert_eq!(response.geometry_version, 17);
        assert_eq!(response.direct.len(), 1);
        assert_eq!(response.direct[0].gain, [0.1, 0.05, 0.02]);
    }

    #[test]
    fn overlapping_voices_keep_independent_fixed_acoustic_origins() {
        let mut input = input(Arc::new(DirectionalTransmission));
        let emitter = input.voices[0].emitter;
        input.voices = vec![
            AcousticVoice {
                voice_id: SourceId::from(41),
                emitter,
                emitter_world_pose: Pose::from_position(Vec3::Z),
                acoustic_priority: 1.0,
                detached: false,
                direct_path: DirectPath::listener_relative(Pose::from_position(-Vec3::Y))
                    .with_geometry(DirectGeometry::BypassTransmission),
                environment_send: EnvironmentSend::from_world_pose(Pose::from_position(Vec3::X)),
            },
            AcousticVoice {
                voice_id: SourceId::from(42),
                emitter,
                emitter_world_pose: Pose::from_position(Vec3::Z),
                acoustic_priority: 1.0,
                detached: false,
                direct_path: DirectPath::listener_relative(Pose::from_position(-Vec3::Y))
                    .with_geometry(DirectGeometry::BypassTransmission),
                environment_send: EnvironmentSend::from_world_pose(Pose::from_position(-Vec3::X)),
            },
        ];

        let response = solve_response(&input, 1.0);
        assert_eq!(response.direct.len(), 2);
        assert_eq!(response.environment_gain(SourceId::from(41)), [0.2; 3]);
        assert_eq!(response.environment_gain(SourceId::from(42)), [0.8; 3]);
        assert_eq!(response.direct_gain(SourceId::from(41)), [1.0; 3]);
        assert_eq!(response.direct_gain(SourceId::from(42)), [1.0; 3]);
    }

    #[test]
    fn open_geometry_has_no_invented_late_reverb() {
        let response = solve_response(&input(Arc::new(NoGeometry)), 1.0);
        assert_eq!(response.late_reverb, LateReverbParameters::SILENT);
    }

    #[test]
    fn enclosed_geometry_produces_bounded_frequency_dependent_reverb() {
        let response = solve_response(&input(Arc::new(UnitRoom)), 1.0);
        assert!(response.late_reverb.wet_gain > 0.0);
        assert!(response.late_reverb.wet_gain <= 0.35);
        assert!(
            response
                .late_reverb
                .rt60_seconds
                .iter()
                .all(|rt60| (0.05..=20.0).contains(rt60))
        );
        assert!(response.late_reverb.rt60_seconds[0] > response.late_reverb.rt60_seconds[2]);
    }

    #[test]
    fn invalid_backend_values_are_sanitized_before_reaching_dsp() {
        let response = solve_response(&input(Arc::new(InvalidMaterialRoom)), 1.0);
        assert_eq!(response.direct[0].gain, [1.0, 0.0, 1.0]);
        assert!(response.late_reverb.wet_gain.is_finite());
        assert!(
            response
                .late_reverb
                .rt60_seconds
                .iter()
                .all(|value| value.is_finite())
        );
    }

    #[test]
    fn visible_first_bounce_paths_produce_bounded_frequency_dependent_taps() {
        let response = solve_response(
            &input(Arc::new(ReflectiveFloor {
                blocks_visibility: false,
            })),
            1.0,
        );
        let taps = &response.direct[0].early_reflections;
        assert!(!taps.is_empty());
        assert!(taps.len() <= MAX_EARLY_REFLECTION_TAPS);
        for tap in taps {
            assert!(tap.arrival_direction.is_finite());
            assert!((0.0..=EARLY_REFLECTION_MAX_DELAY_SECONDS).contains(&tap.delay_seconds));
            assert!(tap.gain[0] > tap.gain[2]);
            assert!(tap.gain.iter().all(|gain| gain.is_finite() && *gain >= 0.0));
        }
    }

    #[test]
    fn blocked_second_segments_reject_early_reflection_candidates() {
        let response = solve_response(
            &input(Arc::new(ReflectiveFloor {
                blocks_visibility: true,
            })),
            1.0,
        );
        assert!(response.direct[0].early_reflections.is_empty());
    }

    #[test]
    fn worker_keeps_only_latest_complete_input_and_closes_cleanly() {
        let propagation =
            AcousticPropagation::new(1.0, Arc::new(AtomicBool::new(true)), 0.5, 8).unwrap();
        propagation
            .publish_scene(Arc::new(AcousticSceneSnapshot::new(17, Arc::new(UnitRoom))))
            .unwrap();
        let emitter = Emitter {
            world_id: 1,
            index: 0,
            generation: 1,
        };

        for revision in [11, 12] {
            propagation
                .publish_spatial_frame(Arc::new(SpatialFrame::new(
                    revision,
                    revision as f64 * 0.01,
                    Pose::from_position(Vec3::ZERO),
                    vec![EmitterSpatialState::new(
                        emitter,
                        Pose::from_position(Vec3::Z),
                    )],
                )))
                .unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let latest_revision = propagation
                .latest_response
                .lock()
                .unwrap()
                .as_ref()
                .map(|response| response.spatial_revision);
            if latest_revision == Some(12) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker did not publish latest input"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        let diagnostics = propagation.diagnostics();
        assert!(diagnostics.solve_count >= 1);
        assert!(diagnostics.published_response_count >= 1);
        assert_eq!(diagnostics.latest_spatial_revision, 12);
        assert_eq!(diagnostics.latest_geometry_version, 17);
        propagation.close();
        assert!(propagation.worker.lock().unwrap().is_none());
    }

    #[test]
    fn disabled_worker_waits_for_reenable_before_solving() {
        let propagation =
            AcousticPropagation::new(1.0, Arc::new(AtomicBool::new(false)), 0.5, 8).unwrap();
        propagation
            .publish_scene(Arc::new(AcousticSceneSnapshot::new(3, Arc::new(UnitRoom))))
            .unwrap();
        propagation
            .publish_spatial_frame(Arc::new(SpatialFrame::new(
                4,
                0.04,
                Pose::from_position(Vec3::ZERO),
                Vec::new(),
            )))
            .unwrap();

        std::thread::sleep(Duration::from_millis(75));
        assert_eq!(propagation.diagnostics().solve_count, 0);

        propagation.set_enabled(true);
        let deadline = Instant::now() + Duration::from_secs(1);
        while propagation.diagnostics().solve_count == 0 {
            assert!(
                Instant::now() < deadline,
                "worker did not resume after reenable"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        propagation.close();
    }

    #[test]
    fn quality_change_wakes_the_existing_worker_without_new_scene_input() {
        let propagation =
            AcousticPropagation::new(1.0, Arc::new(AtomicBool::new(true)), 0.5, 8).unwrap();
        propagation
            .publish_scene(Arc::new(AcousticSceneSnapshot::new(3, Arc::new(UnitRoom))))
            .unwrap();
        propagation
            .publish_spatial_frame(Arc::new(SpatialFrame::new(
                4,
                0.04,
                Pose::from_position(Vec3::ZERO),
                Vec::new(),
            )))
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while propagation.diagnostics().solve_count == 0 {
            assert!(
                Instant::now() < deadline,
                "worker did not solve initial input"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let initial_solve_count = propagation.diagnostics().solve_count;
        let worker_thread_id = propagation
            .worker
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .thread()
            .id();

        propagation.set_quality(1.0);
        assert_eq!(propagation.quality(), 1.0);
        while propagation.diagnostics().solve_count == initial_solve_count {
            assert!(
                Instant::now() < deadline,
                "quality change did not wake the propagation worker"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            propagation
                .worker
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .thread()
                .id(),
            worker_thread_id
        );

        let settled_solve_count = propagation.diagnostics().solve_count;
        propagation.set_quality(1.0);
        std::thread::sleep(Duration::from_millis(75));
        assert_eq!(propagation.diagnostics().solve_count, settled_solve_count);
        propagation.close();
    }
}
