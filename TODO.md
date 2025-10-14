# PetalSonic Improvement Plan

> Based on comparison with reference audionimbus-demo implementation
> Started: 2025-10-14

---

## 🎯 High-Level Goals

### Goal 1: Complete Spatial Audio Feature Set

Implement missing Steam Audio features to match reference implementation:

- [x] Direct path with distance attenuation and air absorption
- [ ] **Occlusion** - sound blocked by geometry
- [ ] **Directivity** - directional sound sources
- [ ] **Reflections** - sound bouncing off surfaces
- [ ] **Reverb** - ambient room acoustics

### Goal 2: Custom Ray Tracer Integration (PRIORITY - DO THIS FIRST)

Enable users to provide their own ray tracing implementation via callbacks:

- [ ] Define callback trait for ray intersection queries
- [ ] Integrate callback-based scene with Steam Audio
- [ ] Support material properties via callback metadata

---

## 🔧 Implementation Plan (Just for reference)

### Phase 1: Custom Ray Tracer Callback API ⭐ PRIORITY

**Status:** Not started

**Objective:** Enable users to provide their own ray tracing implementation via callbacks, allowing PetalSonic to integrate with existing path tracers.

**Reference:** [Steam Audio Scene API - Custom Ray Tracing](https://valvesoftware.github.io/steam-audio/doc/capi/scene.html#ref-scene)

#### Design

Instead of building scene geometry with Steam Audio's API, we provide a simple callback trait that Steam Audio calls when it needs ray intersection data. This lets users reuse their existing ray tracing infrastructure (e.g., game engines, Embree, custom GPU tracers).

**Workflow:**

1. User creates a `MaterialTable` with desired acoustic materials
2. User implements `RayTracer` trait, returning material indices in `RayHit`
3. User calls `world.set_ray_tracer(tracer, materials)`
4. During audio processing, Steam Audio calls the ray tracer for intersection tests
5. PetalSonic looks up material properties from the table using the returned index

```rust
/// Trait for providing custom ray tracing to the spatial audio engine
pub trait RayTracer: Send + Sync {
    /// Test if a ray intersects any geometry
    /// Returns: (hit: bool, distance: f32, material_index: u8, normal: Vec3)
    fn cast_ray(&self, origin: Vec3, direction: Vec3, max_distance: f32)
        -> RayHit;

    /// Called once per frame before any ray casts (optional)
    fn begin_frame(&mut self) {}

    /// Called once per frame after all ray casts (optional)
    fn end_frame(&mut self) {}
}

pub struct RayHit {
    pub hit: bool,
    pub distance: f32,
    pub material_index: u8,  // Index into material table
    pub normal: Vec3,
}
```

**Example Usage:**

```rust
// 1. Create material table
let mut materials = MaterialTable::new();
let wall_mat = materials.add(AudioMaterial::CONCRETE);    // index 0
let floor_mat = materials.add(AudioMaterial::WOOD);       // index 1
let ceiling_mat = materials.add(AudioMaterial::PLASTER);  // index 2

// 2. Implement custom ray tracer
struct MyRayTracer {
    // Your existing scene data...
}

impl RayTracer for MyRayTracer {
    fn cast_ray(&self, origin: Vec3, direction: Vec3, max_distance: f32) -> RayHit {
        // Use your existing ray tracing code here
        if let Some(hit) = self.my_scene.intersect_ray(origin, direction, max_distance) {
            RayHit {
                hit: true,
                distance: hit.distance,
                material_index: hit.surface_material_id,  // Map to your material system
                normal: hit.normal,
            }
        } else {
            RayHit { hit: false, distance: 0.0, material_index: 0, normal: Vec3::ZERO }
        }
    }
}

// 3. Register with PetalSonic
let tracer = MyRayTracer::new(/* ... */);
world.set_ray_tracer(tracer, materials)?;

// 4. Audio now uses your ray tracer for reflections and occlusion!
```

#### Tasks

- [ ] **1.1 Create scene callback module**
  - [ ] Create `petalsonic-core/src/scene/mod.rs`
  - [ ] Create `petalsonic-core/src/scene/ray_tracer.rs`
  - [ ] Create `petalsonic-core/src/scene/material.rs`
  - [ ] Update `petalsonic-core/src/lib.rs` to expose scene module

- [ ] **1.2 Define RayTracer trait and types**
  - [ ] Define `RayTracer` trait with `cast_ray()` method
  - [ ] Define `RayHit` struct
  - [ ] Define `Vec3` helper type (or re-export from existing)
  - [ ] Add optional `begin_frame()` and `end_frame()` hooks

- [ ] **1.3 Implement Material system**

  **Design:** Materials in Steam Audio define acoustic properties across three frequency bands (400 Hz, 2.5 KHz, 15 KHz).

  ```rust
  /// Acoustic properties of a surface material
  /// Matches IPLMaterial from Steam Audio C API
  #[derive(Debug, Clone, Copy)]
  pub struct AudioMaterial {
      /// Fraction of sound energy absorbed at [low, mid, high] frequencies (0.0 - 1.0)
      /// Frequency bands: 400 Hz, 2.5 KHz, 15 KHz
      pub absorption: [f32; 3],

      /// Fraction of sound energy scattered in random direction on reflection (0.0 - 1.0)
      /// 0.0 = pure specular (mirror-like), 1.0 = pure diffuse (scattered)
      pub scattering: f32,

      /// Fraction of sound energy transmitted through surface at [low, mid, high] frequencies (0.0 - 1.0)
      /// Used for direct occlusion calculations
      pub transmission: [f32; 3],
  }

  impl AudioMaterial {
      // Standard material presets
      pub const GENERIC: Self = Self {
          absorption: [0.10, 0.20, 0.30],
          scattering: 0.05,
          transmission: [0.100, 0.050, 0.030],
      };

      pub const BRICK: Self = Self {
          absorption: [0.03, 0.04, 0.07],
          scattering: 0.05,
          transmission: [0.015, 0.015, 0.015],
      };

      pub const CONCRETE: Self = Self {
          absorption: [0.05, 0.07, 0.08],
          scattering: 0.05,
          transmission: [0.015, 0.002, 0.001],
      };

      pub const CERAMIC: Self = Self {
          absorption: [0.01, 0.02, 0.02],
          scattering: 0.05,
          transmission: [0.060, 0.044, 0.011],
      };

      pub const GRAVEL: Self = Self {
          absorption: [0.60, 0.70, 0.80],
          scattering: 0.05,
          transmission: [0.031, 0.012, 0.008],
      };

      pub const CARPET: Self = Self {
          absorption: [0.24, 0.69, 0.73],
          scattering: 0.05,
          transmission: [0.020, 0.005, 0.003],
      };

      pub const GLASS: Self = Self {
          absorption: [0.06, 0.03, 0.02],
          scattering: 0.05,
          transmission: [0.060, 0.044, 0.011],
      };

      pub const PLASTER: Self = Self {
          absorption: [0.12, 0.06, 0.04],
          scattering: 0.05,
          transmission: [0.056, 0.056, 0.004],
      };

      pub const WOOD: Self = Self {
          absorption: [0.11, 0.07, 0.06],
          scattering: 0.05,
          transmission: [0.070, 0.014, 0.005],
      };

      pub const METAL: Self = Self {
          absorption: [0.20, 0.07, 0.06],
          scattering: 0.05,
          transmission: [0.200, 0.025, 0.010],
      };

      pub const ROCK: Self = Self {
          absorption: [0.13, 0.20, 0.24],
          scattering: 0.05,
          transmission: [0.015, 0.002, 0.001],
      };
  }

  /// Material lookup table for ray tracer callbacks
  /// Maps material indices (u8) to AudioMaterial properties
  pub struct MaterialTable {
      materials: Vec<AudioMaterial>,
  }

  impl MaterialTable {
      pub fn new() -> Self { /* ... */ }
      pub fn add(&mut self, material: AudioMaterial) -> u8 { /* returns index */ }
      pub fn get(&self, index: u8) -> Option<&AudioMaterial> { /* ... */ }

      /// Create a table with common presets pre-loaded
      pub fn with_presets() -> Self {
          let mut table = Self::new();
          table.add(AudioMaterial::GENERIC);   // index 0
          table.add(AudioMaterial::BRICK);     // index 1
          table.add(AudioMaterial::CONCRETE);  // index 2
          // ... etc
          table
      }
  }
  ```

  **Implementation tasks:**
  - [ ] Define `AudioMaterial` struct matching IPLMaterial
  - [ ] Add all 11 material presets as constants
  - [ ] Implement `MaterialTable` with add/get methods
  - [ ] Add `with_presets()` helper for common materials
  - [ ] Add conversion to audionimbus/Steam Audio types
  - [ ] Add validation (values must be 0.0-1.0)

- [ ] **1.4 Create C callback bridge to audionimbus**

  Steam Audio's C API expects C function pointers. We need to bridge Rust trait calls:

  - [ ] Create C-compatible callback wrapper functions
  - [ ] Store RayTracer trait object in scene user data
  - [ ] Implement `ClosestHitCallback` bridge (calls `cast_ray()`)
  - [ ] Implement `AnyHitCallback` bridge if needed
  - [ ] Handle panics and errors safely at FFI boundary

- [ ] **1.5 Integrate with PetalSonicWorld**

  ```rust
  impl PetalSonicWorld {
      pub fn set_ray_tracer<T: RayTracer + 'static>(
          &self,
          ray_tracer: T,
          materials: MaterialTable
      ) -> Result<()>

      pub fn clear_ray_tracer(&self) -> Result<()>
  }
  ```

  - [ ] Add scene storage to World (Option<Arc<dyn RayTracer>>)
  - [ ] Pass ray tracer to SpatialProcessor on creation
  - [ ] Ensure thread-safety (RayTracer must be Send + Sync)

- [ ] **1.6 Update SpatialProcessor to use callback scene**
  - [ ] Create audionimbus Scene with custom callbacks
  - [ ] Pass scene to Simulator on creation
  - [ ] Store ray tracer reference in processor
  - [ ] Call `begin_frame()`/`end_frame()` in render loop

- [ ] **1.7 Add tests and simple example ray tracer**
  - [ ] Implement `SimpleBoxRayTracer` for testing (single box room)
  - [ ] Unit tests for callback invocation
  - [ ] Integration test showing rays being cast
  - [ ] Add example to demo with simple geometry

**Files to create:**

- `petalsonic-core/src/scene/mod.rs`
- `petalsonic-core/src/scene/ray_tracer.rs`
- `petalsonic-core/src/scene/material.rs`
- `petalsonic-core/src/scene/simple_box.rs` (example implementation)

**Files to modify:**

- `petalsonic-core/src/lib.rs`
- `petalsonic-core/src/world.rs`
- `petalsonic-core/src/spatial/processor.rs`

**Estimated time:** 3-4 days (FFI bridge is tricky but worth it)

---

### Phase 2: Reflections & Reverb ⭐ COMPLEX

**Status:** Not started

**Objective:** Implement acoustic reflections and reverb for realistic room acoustics.

#### Design Decisions

- **Reflection strategy:** Shared reflection effect (accumulate first, then apply) for efficiency
- **Reverb approach:** Global reverb using listener-based source (matches reference)

#### Tasks

- [ ] **2.1 Add ReflectionEffect to SpatialProcessor**

  ```rust
  pub struct SpatialProcessor {
      // ...existing fields...
      reflection_effect: ReflectionEffect,     // Shared for all sources
      reverb_effect: ReflectionEffect,         // Global reverb
      listener_source: Source,                 // Special source for reverb
      cached_reflection_buf: Vec<f32>,         // (frame_size * 9 channels)
      cached_reverb_buf: Vec<f32>,             // (frame_size * 9 channels)
  }
  ```

  - [ ] Add reflection effect creation in `new()`
  - [ ] Add reverb effect creation in `new()`
  - [ ] Create listener source for reverb simulation
  - [ ] Allocate reflection and reverb buffers

- [ ] **2.2 Update Simulator configuration**
  - [ ] Change from `Simulator<Direct>` to `Simulator<Direct, Reflections>`
  - [ ] Configure ReflectionsSimulationSettings with max rays, duration, etc.
  - [ ] Update builder in `processor.rs:109-117`

- [ ] **2.3 Implement reflection processing pipeline**
  - [ ] Modify `simulate()` to run reflection simulation
  - [ ] Add `process_reflections()` method for each source
  - [ ] Accumulate reflection output to shared buffer
  - [ ] Mix reflection buffer with direct/encoded buffer

- [ ] **2.4 Implement reverb processing**
  - [ ] Update listener source position each frame
  - [ ] Run reverb simulation on listener source
  - [ ] Apply reverb effect to accumulated audio
  - [ ] Mix reverb into final output

- [ ] **2.5 Update processing pipeline**

  ```
  Old: simulate() → process_direct() → encode → decode
  New: simulate() → process_direct() → process_reflections()
       → accumulate → apply_reverb() → decode
  ```

  - [ ] Refactor `process_spatial_sources()` method
  - [ ] Add gain factors for direct/reflections/reverb (configurable)
  - [ ] Ensure proper normalization of mixed signals

- [ ] **2.6 Add configuration options**

  ```rust
  pub struct ReflectionConfig {
      pub enabled: bool,
      pub num_rays: u32,
      pub num_bounces: u32,
      pub duration: f32,
      pub gain: f32,
  }

  pub struct ReverbConfig {
      pub enabled: bool,
      pub gain: f32,
  }
  ```

  - [ ] Add to `PetalSonicWorldDesc`
  - [ ] Apply config in spatial processor

- [ ] **2.7 Testing**
  - [ ] Test with simple room geometry
  - [ ] Verify reflections audible in demo
  - [ ] Test reverb tail behavior
  - [ ] Performance testing with many sources

**Files to modify:**

- `petalsonic-core/src/spatial/processor.rs` (major changes)
- `petalsonic-core/src/spatial/effects.rs`
- `petalsonic-core/src/config/world_desc.rs`

**Estimated time:** 4-5 days

---

### Phase 3: Occlusion & Directivity

**Status:** Not started

**Objective:** Add occlusion (sound blocking) and directional sources.

#### Tasks

- [ ] **3.1 Extend SourceConfig with new fields**

  ```rust
  pub struct SpatialSourceConfig {
      pub position: Vec3,
      pub volume: f32,
      pub directivity: Option<DirectivityPattern>,
      pub occlusion_enabled: bool,
      pub occlusion_rays: u32,
  }

  pub enum DirectivityPattern {
      Omnidirectional,
      Cardioid { orientation: Vec3, sharpness: f32 },
      Dipole { orientation: Vec3 },
      Custom { /* callback or pattern data */ },
  }
  ```

  - [ ] Define DirectivityPattern enum
  - [ ] Add builder methods to SourceConfig
  - [ ] Maintain backward compatibility

- [ ] **3.2 Implement directivity in simulation**
  - [ ] Convert DirectivityPattern to audionimbus::Directivity
  - [ ] Apply in `SimulationInputs` in `simulate()`
  - [ ] Test with cardioid pattern (speakers, voices)

- [ ] **3.3 Implement occlusion**
  - [ ] Enable occlusion in SimulationInputs when configured
  - [ ] Set num_transmission_rays from config
  - [ ] Apply occlusion parameters in DirectEffect
  - [ ] Test with geometry blocking line of sight

- [ ] **3.4 Add demo examples**
  - [ ] Example with directional speaker
  - [ ] Example showing occlusion behind wall
  - [ ] Visualize directivity pattern in GUI

**Files to modify:**

- `petalsonic-core/src/config/source_config.rs`
- `petalsonic-core/src/spatial/processor.rs`

**Estimated time:** 2-3 days

---

### Phase 4: Profiling Integration

**Status:** Not started

**Objective:** Add detailed timing metrics for spatial audio processing.

#### Tasks

- [ ] **4.1 Extend RenderTimingEvent**

  ```rust
  pub struct RenderTimingEvent {
      pub mixing_time_us: u64,
      pub spatial_time_us: u64,
      pub resampling_time_us: u64,
      pub total_time_us: u64,
      // NEW:
      pub spatial_direct_time_us: u64,
      pub spatial_reflections_time_us: u64,
      pub spatial_reverb_time_us: u64,
      pub spatial_encode_time_us: u64,
      pub spatial_decode_time_us: u64,
  }
  ```

  - [ ] Add new timing fields
  - [ ] Update event creation in engine

- [ ] **4.2 Add timing instrumentation in SpatialProcessor**
  - [ ] Time direct effect processing
  - [ ] Time reflection processing
  - [ ] Time reverb processing
  - [ ] Time ambisonics encode/decode
  - [ ] Aggregate and return timing data

- [ ] **4.3 Update mixer to collect spatial timing**
  - [ ] Pass timing data from spatial processor
  - [ ] Include in RenderTimingEvent
  - [ ] Emit via timing channel

- [ ] **4.4 Update profiling GUI**
  - [ ] Add new graph lines for spatial breakdown
  - [ ] Color-code different spatial stages
  - [ ] Show percentage of time in each stage
  - [ ] Add legend for new metrics

**Files to modify:**

- `petalsonic-core/src/events.rs`
- `petalsonic-core/src/spatial/processor.rs`
- `petalsonic-core/src/mixer.rs`
- `petalsonic-demo/src/gui/profiling.rs`

**Estimated time:** 1 day

---

### Phase 5: Convenience API

**Status:** Not started

**Objective:** Make common spatial audio tasks easier with helper methods and presets.

#### Tasks

- [ ] **5.1 Add example ray tracer implementations**

  ```rust
  // Provide ready-to-use ray tracers for common scenarios:
  - SimpleBoxRayTracer::new(width, height, depth, material)
  - SimpleRoomRayTracer::with_walls(...)
  - EmptyRayTracer::new() // No geometry, for testing
  ```

  - [ ] Implement common ray tracer examples
  - [ ] Document ray tracer implementation guide
  - [ ] Show how to integrate with popular ray tracing libraries

- [ ] **5.2 Add SourceConfig builder pattern**

  ```rust
  SourceConfig::spatial(position)
      .with_volume(0.8)
      .with_directivity(DirectivityPattern::Cardioid {
          orientation: Vec3::new(0.0, 0.0, -1.0),
          sharpness: 0.5
      })
      .with_occlusion(true)
  ```

  - [ ] Implement builder methods
  - [ ] Ensure ergonomic API
  - [ ] Add documentation with examples

- [ ] **5.3 Add convenience methods for common operations**
  - [ ] `world.play_spatial(audio_id, position)` - one-liner for spatial playback
  - [ ] `world.play_spatial_looped(audio_id, position)` - looping variant
  - [ ] `world.update_source_position(audio_id, position)` - shortcut for position update

- [ ] **5.4 Documentation and examples**
  - [ ] Update README with new features
  - [ ] Add spatial audio guide
  - [ ] Create example gallery:
    - Simple room with reflections
    - Cathedral with long reverb
    - Directional speaker demo
    - Occlusion demo (door opening)

**Files to modify:**

- `petalsonic-core/src/world.rs`
- `petalsonic-core/src/config/source_config.rs`
- `README.md`
- `petalsonic-demo/src/main.rs` (examples)

**Estimated time:** 1 day

---

## 🗺️ Architecture Decisions

### Reflection Effect Allocation

**Decision:** Shared reflection effect (accumulate then apply)
**Rationale:**

- Memory efficient - single effect vs N per-source effects
- CPU efficient - one convolution pass
- Acceptable quality trade-off for typical use cases
- Can revisit per-source reflections if needed later

### Scene Geometry Integration

**Decision:** Callback-based ray tracing instead of built-in geometry
**Rationale:**

- Users can integrate their existing ray tracers (game engine, Embree, GPU-based)
- No need to duplicate scene data in PetalSonic
- More flexible - works with any ray tracing backend
- Follows Steam Audio's recommended pattern for advanced use cases
- Simpler API surface - just implement one trait

### Reverb Architecture

**Decision:** Global reverb with listener-based source
**Rationale:**

- Matches reference implementation pattern
- Efficient - single reverb effect
- Good for most use cases
- Per-zone reverb can be added as enhancement

### API Changes

**Decision:** Builder pattern for SourceConfig
**Rationale:**

- Maintains backward compatibility
- Ergonomic API
- Easy to extend with new features
- Clear intent in code

### Implementation Order

**Decision:** Incremental, phase-by-phase
**Rationale:**

- Verify each feature works before moving on
- Easier to test and debug
- Can get feedback early
- Lower risk

---

## 📊 Progress Tracking

### Overall Progress: 10% Complete

| Phase | Status | Progress | Estimated Time | Notes |
|-------|--------|----------|----------------|-------|
| Phase 1: Ray Tracer Callbacks | 🔴 Not Started | 0% | 3-4 days | Foundation for custom ray tracers |
| Phase 2: Reflections & Reverb | 🔴 Not Started | 0% | 4-5 days | Most complex phase |
| Phase 3: Occlusion & Directivity | 🔴 Not Started | 0% | 2-3 days | Depends on Phase 1 |
| Phase 4: Profiling | 🔴 Not Started | 0% | 1 day | Can be done anytime |
| Phase 5: Convenience API | 🔴 Not Started | 0% | 1 day | Polish and UX |

**Legend:**

- 🔴 Not Started
- 🟡 In Progress
- 🟢 Complete
- ⏸️ Blocked/Paused

---

## 📝 Technical Notes

### Material System Design

**Frequency Bands:** Steam Audio uses three frequency bands for acoustic simulations:

- **Low (400 Hz)**: Bass/low-frequency sounds
- **Mid (2.5 KHz)**: Most speech and mid-range content
- **High (15 KHz)**: High-frequency content and detail

**Properties:**

- **Absorption**: How much sound energy is absorbed (not reflected) by the surface
  - Higher values = "softer" surface (carpet, fabric absorb more)
  - Lower values = "harder" surface (metal, glass reflect more)
- **Scattering**: How diffuse vs. specular the reflection is
  - 0.0 = perfect mirror reflection (rare in reality)
  - 1.0 = completely scattered/diffuse reflection
  - Most materials use 0.05 for slight diffusion
- **Transmission**: How much sound passes through the material (for occlusion)
  - Used when sound source is behind an obstacle
  - Higher values = more transparent (thin wood, glass)
  - Lower values = more blocking (concrete, metal)

### Current Architecture Strengths

- ✅ Real-time safe design (lock-free ring buffer)
- ✅ Dedicated audio threads
- ✅ Pre-allocated buffers
- ✅ Supports resampling
- ✅ Framework-agnostic

### Current Limitations

- ❌ No reflections/reverb (no ray tracer integration yet)
- ❌ No occlusion (sources omnidirectional)
- ❌ No directivity
- ❌ Basic spatial config (position + volume only)
- ❌ No callback API for custom ray tracers

### Reference Implementation Advantages

- Uses Bevy ECS for source management
- Full Steam Audio feature demonstration
- Has reflections, reverb, occlusion
- Complex scene geometry (corridors, cathedral)
- Per-source reflection effects

### Performance Targets

- Direct path: < 100µs per source
- Reflections: < 500µs per frame (all sources)
- Reverb: < 200µs per frame
- Total spatial: < 1ms per frame @ 48kHz (1024 samples = 21.3ms budget)
- Target utilization: < 5% of frame time for spatial audio

---

## 🚀 Getting Started

### To begin Phase 1

1. Define the `RayTracer` trait and `RayHit` struct
2. Implement `Material` and `MaterialTable` types
3. Create FFI bridge to connect Rust callbacks to Steam Audio's C API
4. Integrate callback scene with `PetalSonicWorld` and `SpatialProcessor`
5. Build `SimpleBoxRayTracer` example implementation
6. Test with reflection-based audio in demo

### Testing Strategy

- Unit tests for each component
- Integration tests for end-to-end pipeline
- Audio quality verification (manual listening tests)
- Performance benchmarks at each phase
- Regression testing with existing functionality

---

## 📚 References

- [Steam Audio Documentation](https://valvesoftware.github.io/steam-audio/)
- [Steam Audio Scene API - Custom Ray Tracing](https://valvesoftware.github.io/steam-audio/doc/capi/scene.html#ref-scene)
- [Steam Audio Material Properties](https://valvesoftware.github.io/steam-audio/doc/capi/material.html)
- [AudioNimbus Rust Bindings](https://github.com/MaxenceMaire/audionimbus)
- [Reference Implementation](petalsonic-core/src/reference-audio/)
- Comparison Analysis - see analysis above

---

## 🔮 Future Ideas (Beyond Current Plan)

### Advanced Features (Planned for Future)

#### Core

- [ ] Built-in scene geometry API (StaticMeshBuilder) as alternative to callbacks
- [ ] Device switch handling
- [ ] Per-zone reverb (room transitions)
- [ ] Baked reflection data support
- [ ] GPU-accelerated ray tracing via Radeon Rays
- [ ] Integration examples for Embree and other ray tracers

#### Demo

- [ ] Web UI with static audio source positioning
- [ ] Interactive drag-and-drop with real-time audio updates
- [ ] 3D visualization of acoustic scene
- [ ] Real-time HRTF customization
