use crate::math::Vec3;

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

/// Host-provided batched any-hit ray tracing backend used by Steam Audio custom scenes.
pub trait BatchedAnyHitRayTracer: Send + Sync {
    fn trace_any_hit_batch(
        &self,
        rays: &[AcousticRay],
        min_distances: &[f32],
        max_distances: &[f32],
    ) -> Vec<bool>;
}

/// Host-provided batched closest-hit ray tracing backend used by reflections.
pub trait BatchedClosestHitRayTracer: Send + Sync {
    fn trace_closest_hit_batch(
        &self,
        rays: &[AcousticRay],
        min_distances: &[f32],
        max_distances: &[f32],
    ) -> Vec<Option<AcousticHit>>;
}
