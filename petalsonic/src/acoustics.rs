use crate::math::Vec3;

/// A world-space acoustics ray.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcousticRay {
    pub origin: Vec3,
    pub direction: Vec3,
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
