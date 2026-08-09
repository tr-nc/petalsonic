use crate::math::Vec3;
use std::sync::{Arc, RwLock};

/// A world-space acoustics ray.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcousticRay {
    pub origin: Vec3,
    pub direction: Vec3,
}

/// Acoustic material properties used by custom ray tracing backends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcousticMaterial {
    pub absorption: [f32; 3],
    pub scattering: f32,
    pub transmission: [f32; 3],
}

impl Default for AcousticMaterial {
    fn default() -> Self {
        Self {
            absorption: [0.10, 0.20, 0.30],
            scattering: 0.05,
            transmission: [0.100, 0.050, 0.030],
        }
    }
}

/// Closest-hit result for an acoustics ray.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcousticHit {
    pub distance: f32,
    pub normal: Vec3,
    pub material: AcousticMaterial,
}

/// Immutable batched any-hit query backend captured by an acoustic-scene snapshot.
///
/// PetalSonic invokes this on its render thread with caller-provided result storage;
/// implementations must have bounded work and must not allocate, block, or access mutable
/// game-world state.
pub trait BatchedAnyHitRayTracer: Send + Sync {
    fn trace_any_hit_batch(
        &self,
        rays: &[AcousticRay],
        min_distances: &[f32],
        max_distances: &[f32],
        hits: &mut [bool],
    );
}

/// Immutable batched closest-hit query backend captured by an acoustic-scene snapshot.
///
/// PetalSonic invokes this on its render thread with caller-provided result storage;
/// implementations must have bounded work and must not allocate, block, or access mutable
/// game-world state.
pub trait BatchedClosestHitRayTracer: Send + Sync {
    fn trace_closest_hit_batch(
        &self,
        rays: &[AcousticRay],
        min_distances: &[f32],
        max_distances: &[f32],
        hits: &mut [Option<AcousticHit>],
    );
}

/// Immutable, shareable acoustic-scene version.
///
/// The contained query backends are expected to share immutable BVH chunks with the host;
/// publishing a new snapshot swaps this small handle and does not deep-copy geometry.
#[derive(Clone)]
pub struct AcousticSceneSnapshot {
    version: u64,
    any_hit: Option<Arc<dyn BatchedAnyHitRayTracer>>,
    closest_hit: Option<Arc<dyn BatchedClosestHitRayTracer>>,
}

impl AcousticSceneSnapshot {
    pub fn new(
        version: u64,
        any_hit: Option<Arc<dyn BatchedAnyHitRayTracer>>,
        closest_hit: Option<Arc<dyn BatchedClosestHitRayTracer>>,
    ) -> Self {
        Self {
            version,
            any_hit,
            closest_hit,
        }
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn supports_occlusion(&self) -> bool {
        self.any_hit.is_some()
    }

    pub fn supports_reflections(&self) -> bool {
        self.closest_hit.is_some()
    }
}

impl std::fmt::Debug for AcousticSceneSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcousticSceneSnapshot")
            .field("version", &self.version)
            .field("supports_occlusion", &self.supports_occlusion())
            .field("supports_reflections", &self.supports_reflections())
            .finish()
    }
}

/// Stable adapter captured by the spatial backend while scene versions change behind it.
pub(crate) struct AcousticSceneSlot {
    active: RwLock<Option<Arc<AcousticSceneSnapshot>>>,
}

impl AcousticSceneSlot {
    pub(crate) fn new(initial: Option<Arc<AcousticSceneSnapshot>>) -> Self {
        Self {
            active: RwLock::new(initial),
        }
    }

    pub(crate) fn replace(
        &self,
        next: Option<Arc<AcousticSceneSnapshot>>,
    ) -> std::result::Result<Option<Arc<AcousticSceneSnapshot>>, Option<Arc<AcousticSceneSnapshot>>>
    {
        let Ok(mut active) = self.active.try_write() else {
            return Err(next);
        };
        Ok(std::mem::replace(&mut *active, next))
    }
}

impl BatchedAnyHitRayTracer for AcousticSceneSlot {
    fn trace_any_hit_batch(
        &self,
        rays: &[AcousticRay],
        min_distances: &[f32],
        max_distances: &[f32],
        hits: &mut [bool],
    ) {
        hits.fill(false);
        let _ = self.active.try_read().ok().and_then(|snapshot| {
            snapshot.as_ref().and_then(|snapshot| {
                snapshot.any_hit.as_ref().map(|backend| {
                    backend.trace_any_hit_batch(rays, min_distances, max_distances, hits)
                })
            })
        });
    }
}

impl BatchedClosestHitRayTracer for AcousticSceneSlot {
    fn trace_closest_hit_batch(
        &self,
        rays: &[AcousticRay],
        min_distances: &[f32],
        max_distances: &[f32],
        hits: &mut [Option<AcousticHit>],
    ) {
        hits.fill(None);
        let _ = self.active.try_read().ok().and_then(|snapshot| {
            snapshot.as_ref().and_then(|snapshot| {
                snapshot.closest_hit.as_ref().map(|backend| {
                    backend.trace_closest_hit_batch(rays, min_distances, max_distances, hits)
                })
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct AlwaysHit;

    impl BatchedAnyHitRayTracer for AlwaysHit {
        fn trace_any_hit_batch(
            &self,
            rays: &[AcousticRay],
            _min_distances: &[f32],
            _max_distances: &[f32],
            hits: &mut [bool],
        ) {
            hits[..rays.len()].fill(true);
        }
    }

    #[test]
    fn scene_versions_swap_shallow_shared_query_backends() {
        let backend: Arc<dyn BatchedAnyHitRayTracer> = Arc::new(AlwaysHit);
        let snapshot = AcousticSceneSnapshot::new(1, Some(backend.clone()), None);
        let slot = AcousticSceneSlot::new(Some(Arc::new(snapshot.clone())));
        let rays = [AcousticRay {
            origin: Vec3::ZERO,
            direction: Vec3::X,
        }];

        let mut hits = [false];
        slot.trace_any_hit_batch(&rays, &[0.0], &[10.0], &mut hits);
        assert_eq!(hits, [true]);
        assert!(Arc::strong_count(&backend) >= 3);

        let previous = slot
            .replace(Some(Arc::new(AcousticSceneSnapshot::new(
                2,
                Some(backend.clone()),
                None,
            ))))
            .unwrap()
            .unwrap();
        assert_eq!(previous.version(), 1);
        slot.trace_any_hit_batch(&rays, &[0.0], &[10.0], &mut hits);
        assert_eq!(hits, [true]);
    }

    struct DropTrackedBackend {
        dropped_on: Arc<Mutex<Option<std::thread::ThreadId>>>,
    }

    impl Drop for DropTrackedBackend {
        fn drop(&mut self) {
            *self.dropped_on.lock().unwrap() = Some(std::thread::current().id());
        }
    }

    impl BatchedAnyHitRayTracer for DropTrackedBackend {
        fn trace_any_hit_batch(
            &self,
            _rays: &[AcousticRay],
            _min_distances: &[f32],
            _max_distances: &[f32],
            hits: &mut [bool],
        ) {
            hits.fill(false);
        }
    }

    #[test]
    fn replaced_snapshot_is_returned_for_non_render_thread_destruction() {
        let dropped_on = Arc::new(Mutex::new(None));
        let backend: Arc<dyn BatchedAnyHitRayTracer> = Arc::new(DropTrackedBackend {
            dropped_on: dropped_on.clone(),
        });
        let slot = AcousticSceneSlot::new(Some(Arc::new(AcousticSceneSnapshot::new(
            1,
            Some(backend),
            None,
        ))));

        let retired = slot.replace(None).unwrap().unwrap();
        assert!(dropped_on.lock().unwrap().is_none());
        let producer_thread = std::thread::current().id();
        drop(retired);

        assert_eq!(*dropped_on.lock().unwrap(), Some(producer_thread));
    }
}
