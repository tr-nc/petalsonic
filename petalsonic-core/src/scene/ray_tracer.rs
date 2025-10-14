//! Ray tracing callback trait for spatial audio simulation.
//!
//! This module provides the interface for users to implement custom ray tracing
//! for spatial audio effects like reflections, reverb, and occlusion.

use crate::math::Vec3;

/// Result of a ray intersection test.
///
/// Returned by `RayTracer::cast_ray()` to provide hit information to the
/// spatial audio engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RayHit {
    /// Whether the ray hit any geometry
    pub hit: bool,

    /// Distance from ray origin to hit point (in meters)
    ///
    /// Only meaningful if `hit` is true
    pub distance: f32,

    /// Index into the material table for the hit surface
    ///
    /// Used to look up acoustic properties (absorption, scattering, transmission)
    /// Only meaningful if `hit` is true
    pub material_index: u8,

    /// Surface normal at the hit point (normalized)
    ///
    /// Used for reflection calculations
    /// Only meaningful if `hit` is true
    pub normal: Vec3,
}

impl RayHit {
    /// Creates a miss result (no hit)
    pub fn miss() -> Self {
        Self {
            hit: false,
            distance: 0.0,
            material_index: 0,
            normal: Vec3::new(0.0, 0.0, 0.0),
        }
    }

    /// Creates a hit result
    pub fn new(distance: f32, material_index: u8, normal: Vec3) -> Self {
        Self {
            hit: true,
            distance,
            material_index,
            normal,
        }
    }
}

impl Default for RayHit {
    fn default() -> Self {
        Self::miss()
    }
}

/// Trait for providing custom ray tracing to the spatial audio engine.
///
/// Implement this trait to integrate your own ray tracing system with PetalSonic.
/// This allows you to use existing path tracers (game engines, Embree, GPU tracers, etc.)
/// for spatial audio calculations.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync` as they will be called from the audio thread.
/// Consider using interior mutability (e.g., `Arc<Mutex<T>>`) if you need to share
/// mutable state with other threads.
///
/// # Performance
///
/// The audio engine may call `cast_ray()` hundreds of times per frame for reflection
/// and occlusion calculations. Optimize your ray tracing code accordingly.
///
/// # Example
///
/// ```
/// use petalsonic_core::math::Vec3;
/// use petalsonic_core::scene::{RayTracer, RayHit};
///
/// struct SimpleBoxTracer {
///     min: Vec3,
///     max: Vec3,
///     wall_material_index: u8,
/// }
///
/// impl RayTracer for SimpleBoxTracer {
///     fn cast_ray(&self, origin: Vec3, direction: Vec3, max_distance: f32) -> RayHit {
///         // Implement box intersection
///         // This is a simplified example - real implementation would be more complex
///         if let Some((t, normal)) = self.intersect_box(origin, direction, max_distance) {
///             RayHit::new(t, self.wall_material_index, normal)
///         } else {
///             RayHit::miss()
///         }
///     }
/// }
///
/// impl SimpleBoxTracer {
///     fn intersect_box(&self, origin: Vec3, direction: Vec3, max_distance: f32)
///         -> Option<(f32, Vec3)> {
///         // Box intersection logic here
///         None
///     }
/// }
/// ```
pub trait RayTracer: Send + Sync {
    /// Test if a ray intersects any geometry.
    ///
    /// # Parameters
    ///
    /// * `origin` - Ray starting position in world space (meters)
    /// * `direction` - Ray direction (should be normalized)
    /// * `max_distance` - Maximum ray distance to test (meters)
    ///
    /// # Returns
    ///
    /// * `RayHit` - Information about the closest hit, or a miss if no hit occurred
    ///
    /// # Notes
    ///
    /// - If multiple surfaces are hit, return the **closest** hit
    /// - The surface normal should point **away** from the surface (outward)
    /// - Material index should map to a material in the `MaterialTable` provided to the world
    fn cast_ray(&self, origin: Vec3, direction: Vec3, max_distance: f32) -> RayHit;

    /// Called once per audio frame before any ray casts (optional).
    ///
    /// Use this to prepare your ray tracer for a batch of queries, e.g.:
    /// - Update acceleration structures
    /// - Reset statistics/counters
    /// - Synchronize with game state
    ///
    /// Default implementation does nothing.
    fn begin_frame(&mut self) {}

    /// Called once per audio frame after all ray casts (optional).
    ///
    /// Use this to clean up after a batch of queries, e.g.:
    /// - Log statistics
    /// - Release temporary resources
    ///
    /// Default implementation does nothing.
    fn end_frame(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ray_hit_miss() {
        let miss = RayHit::miss();
        assert!(!miss.hit);
        assert_eq!(miss.distance, 0.0);
    }

    #[test]
    fn test_ray_hit_new() {
        let normal = Vec3::new(0.0, 1.0, 0.0);
        let hit = RayHit::new(5.0, 2, normal);
        assert!(hit.hit);
        assert_eq!(hit.distance, 5.0);
        assert_eq!(hit.material_index, 2);
        assert_eq!(hit.normal, normal);
    }

    // Simple test ray tracer that always returns a miss
    struct NoopTracer;

    impl RayTracer for NoopTracer {
        fn cast_ray(&self, _origin: Vec3, _direction: Vec3, _max_distance: f32) -> RayHit {
            RayHit::miss()
        }
    }

    #[test]
    fn test_noop_tracer() {
        let tracer = NoopTracer;
        let origin = Vec3::new(0.0, 0.0, 0.0);
        let direction = Vec3::new(0.0, 0.0, 1.0);
        let result = tracer.cast_ray(origin, direction, 100.0);
        assert!(!result.hit);
    }
}
