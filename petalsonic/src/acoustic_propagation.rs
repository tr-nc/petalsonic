use crate::acoustics::{AcousticMaterial, AcousticRay, AcousticSceneSnapshot};
use crate::domain::{Emitter, EmitterSpatialState, SpatialFrame};
use crate::math::Vec3;
use crate::spatial::LateReverbParameters;
use std::cmp::Ordering as CmpOrdering;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const SPEED_OF_SOUND_METERS_PER_SECOND: f32 = 343.0;
const SOLVE_INTERVAL: Duration = Duration::from_millis(33);
const MAX_DIRECT_SOURCES: usize = 32;
const LATE_RAY_COUNT: usize = 256;
const LATE_BOUNCE_COUNT: usize = 8;
const MAX_TRACE_DISTANCE_METERS: f32 = 120.0;
const RAY_EPSILON_METERS: f32 = 0.05;

#[derive(Clone, Copy, Debug)]
pub(crate) struct DirectAcousticResponse {
    pub emitter: Emitter,
    pub gain: [f32; 3],
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
    pub(crate) fn direct_gain(&self, emitter: Emitter) -> [f32; 3] {
        self.direct
            .iter()
            .find(|response| response.emitter == emitter)
            .map(|response| response.gain)
            .unwrap_or([1.0; 3])
    }
}

#[derive(Clone)]
struct SolveInput {
    generation: u64,
    spatial: Arc<SpatialFrame>,
    scene: Arc<AcousticSceneSnapshot>,
}

#[derive(Default)]
struct InputState {
    generation: u64,
    spatial: Option<Arc<SpatialFrame>>,
    scene: Option<Arc<AcousticSceneSnapshot>>,
}

impl InputState {
    fn capture(&self) -> Option<SolveInput> {
        Some(SolveInput {
            generation: self.generation,
            spatial: self.spatial.clone()?,
            scene: self.scene.clone()?,
        })
    }
}

struct SharedInput {
    state: Mutex<InputState>,
    changed: Condvar,
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
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl AcousticPropagation {
    pub(crate) fn new(distance_scaler: f32) -> std::io::Result<Self> {
        let input = Arc::new(SharedInput {
            state: Mutex::new(InputState::default()),
            changed: Condvar::new(),
        });
        let latest_response = Arc::new(Mutex::new(None));
        let counters = Arc::new(AcousticPropagationCounters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let input = input.clone();
            let latest_response = latest_response.clone();
            let counters = counters.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("petalsonic-acoustics".into())
                .spawn(move || {
                    propagation_loop(&input, &latest_response, &counters, &stop, distance_scaler)
                })?
        };
        Ok(Self {
            input,
            latest_response,
            counters,
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

    pub(crate) fn diagnostics(&self) -> AcousticPropagationDiagnostics {
        self.counters.snapshot()
    }

    pub(crate) fn close(&self) {
        self.stop.store(true, Ordering::Release);
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
    AcousticResponse {
        spatial_revision: input.spatial.revision(),
        geometry_version: input.scene.version(),
        direct: solve_direct(input, distance_scaler),
        late_reverb: solve_late_reverb(input, distance_scaler),
        published_at: Instant::now(),
        solve_time_us: 0,
    }
}

fn solve_direct(input: &SolveInput, distance_scaler: f32) -> Vec<DirectAcousticResponse> {
    let listener = input.spatial.listener().position;
    let ray_epsilon_world = RAY_EPSILON_METERS / distance_scaler.max(0.001);
    let mut candidates: Vec<(f32, &EmitterSpatialState)> = input
        .spatial
        .emitters()
        .iter()
        .filter_map(|emitter| {
            let distance = emitter.pose.position.distance(listener);
            let priority = emitter.acoustic_priority();
            (distance.is_finite() && priority.is_finite() && priority > 0.0)
                .then_some((priority / (1.0 + distance), emitter))
        })
        .collect();
    candidates.sort_by(|left, right| right.0.partial_cmp(&left.0).unwrap_or(CmpOrdering::Equal));
    candidates.truncate(MAX_DIRECT_SOURCES);

    let mut rays = Vec::with_capacity(candidates.len());
    let mut min_distances = Vec::with_capacity(candidates.len());
    let mut max_distances = Vec::with_capacity(candidates.len());
    for (_, emitter) in &candidates {
        let delta = emitter.pose.position - listener;
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

    candidates
        .into_iter()
        .zip(hits)
        .zip(min_distances.into_iter().zip(max_distances))
        .map(
            |(((_, emitter), hit), (min_distance, max_distance))| DirectAcousticResponse {
                emitter: emitter.emitter,
                gain: hit
                    .filter(|hit| valid_hit_distance(hit.distance, min_distance, max_distance))
                    .map(|hit| {
                        hit.material
                            .transmission
                            .map(|gain| sanitize_unit(gain, 1.0))
                    })
                    .unwrap_or([1.0; 3]),
            },
        )
        .collect()
}

fn solve_late_reverb(input: &SolveInput, distance_scaler: f32) -> LateReverbParameters {
    let listener = input.spatial.listener().position;
    let max_distance_world = MAX_TRACE_DISTANCE_METERS / distance_scaler.max(0.001);
    let ray_epsilon_world = RAY_EPSILON_METERS / distance_scaler.max(0.001);
    let mut rays: Vec<AcousticRay> = (0..LATE_RAY_COUNT)
        .map(|index| AcousticRay {
            origin: listener,
            direction: fibonacci_direction(index, LATE_RAY_COUNT),
        })
        .collect();
    let min_distances = vec![ray_epsilon_world; LATE_RAY_COUNT];
    let max_distances = vec![max_distance_world; LATE_RAY_COUNT];
    let mut hits = vec![None; LATE_RAY_COUNT];
    let mut active = vec![true; LATE_RAY_COUNT];
    let mut energy = vec![[1.0f32; 3]; LATE_RAY_COUNT];
    let mut hit_segments = 0usize;
    let mut first_bounce_hits = 0usize;
    let mut minimum_hit_distance_meters = f32::INFINITY;
    let mut segment_time_sum = 0.0f32;
    let mut log_reflectivity_sum = [0.0f32; 3];
    let mut reflected_energy_sum = [0.0f32; 3];

    for bounce in 0..LATE_BOUNCE_COUNT {
        hits.fill(None);
        input.scene.query().trace_closest_hit_batch(
            &rays,
            &min_distances,
            &max_distances,
            &mut hits,
        );
        for index in 0..LATE_RAY_COUNT {
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
    let enclosure = first_bounce_hits as f32 / LATE_RAY_COUNT as f32;
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
    fn worker_keeps_only_latest_complete_input_and_closes_cleanly() {
        let propagation = AcousticPropagation::new(1.0).unwrap();
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
}
