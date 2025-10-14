//! Scene management and ray tracing for spatial audio.
//!
//! This module provides the interface for integrating custom ray tracing with
//! PetalSonic's spatial audio engine. Users can implement the `RayTracer` trait
//! to provide geometry and material information for reflections, reverb, and occlusion.
//!
//! # Overview
//!
//! The scene system consists of three main components:
//!
//! 1. **RayTracer** - Trait for providing ray intersection queries
//! 2. **AudioMaterial** - Acoustic properties of surfaces (absorption, scattering, transmission)
//! 3. **MaterialTable** - Maps material indices to their acoustic properties
//!
//! # Workflow
//!
//! 1. Create a `MaterialTable` with desired acoustic materials
//! 2. Implement the `RayTracer` trait, returning material indices in `RayHit`
//! 3. Register the ray tracer with `PetalSonicWorld::set_ray_tracer()`
//! 4. The audio engine calls your ray tracer during spatial processing
//!
//! # Example
//!
//! ```rust,ignore
//! use petalsonic_core::scene::{AudioMaterial, MaterialTable, RayTracer, RayHit};
//! use petalsonic_core::math::Vec3;
//!
//! // 1. Create material table
//! let mut materials = MaterialTable::new();
//! let wall_mat = materials.add(AudioMaterial::CONCRETE);
//! let floor_mat = materials.add(AudioMaterial::WOOD);
//!
//! // 2. Implement custom ray tracer
//! struct MyRayTracer {
//!     // Your scene data...
//! }
//!
//! impl RayTracer for MyRayTracer {
//!     fn cast_ray(&self, origin: Vec3, direction: Vec3, max_distance: f32) -> RayHit {
//!         // Use your existing ray tracing code
//!         // ...
//!         RayHit::miss()
//!     }
//! }
//!
//! // 3. Register with PetalSonic
//! let tracer = MyRayTracer { /* ... */ };
//! world.set_ray_tracer(tracer, materials)?;
//! ```

pub mod material;
pub mod ray_tracer;

pub use material::{AudioMaterial, MaterialTable};
pub use ray_tracer::{RayHit, RayTracer};
