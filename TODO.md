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

### Goal 2: Scene Geometry Management

Enable users to define acoustic environments:

- [ ] Add/remove static meshes to the scene
- [ ] Support different material properties (wood, concrete, glass, etc.)
- [ ] Runtime geometry updates
- [ ] Simple API for common room shapes

### Goal 3: Enhanced Source Configuration

Extend `SourceConfig` with more spatial properties:

- [ ] Directivity patterns (omnidirectional, cardioid, dipole)
- [ ] Per-source occlusion settings
- [ ] Per-source reflection/reverb mix controls

### Goal 4: Performance Profiling Enhancement

Add spatial-specific profiling:

- [ ] Track reflection computation time separately
- [ ] Monitor per-source processing cost
- [ ] Measure geometry complexity impact
- [ ] Add spatial breakdown to GUI profiler

### Goal 5: API Usability

Make it easier to use spatial audio:

- [ ] Scene builder pattern for quick setups
- [ ] Preset acoustic environments (room, hall, cathedral)
- [ ] Helper methods for common operations

---

## 🔧 Implementation Plan

### Phase 1: Foundation - Scene Geometry API ⭐ PRIORITY

**Status:** Not started

**Objective:** Enable users to add 3D geometry to the acoustic scene for reflections and occlusion.

#### Tasks

- [ ] **1.1 Create scene geometry module structure**
  - [ ] Create `petalsonic-core/src/scene/mod.rs`
  - [ ] Create `petalsonic-core/src/scene/geometry.rs`
  - [ ] Create `petalsonic-core/src/scene/material.rs`
  - [ ] Create `petalsonic-core/src/scene/builder.rs`
  - [ ] Update `petalsonic-core/src/lib.rs` to expose scene module

- [ ] **1.2 Implement Material types**
  - [ ] Define `Material` enum with presets (Wood, Concrete, Glass, etc.)
  - [ ] Add custom material support with absorption coefficients
  - [ ] Add material conversion to audionimbus types

- [ ] **1.3 Implement StaticMeshBuilder**

  ```rust
  // API design:
  StaticMeshBuilder::new()
      .add_box(center, size)
      .add_plane(position, normal, size)
      .with_material(Material::CONCRETE)
      .build()?
  ```

  - [ ] Basic vertex/triangle API
  - [ ] Box primitive helper
  - [ ] Plane primitive helper
  - [ ] Material assignment per triangle or per mesh

- [ ] **1.4 Integrate with PetalSonicWorld**

  ```rust
  impl PetalSonicWorld {
      pub fn add_static_mesh(&self, builder: StaticMeshBuilder) -> Result<MeshId>
      pub fn remove_static_mesh(&self, mesh_id: MeshId) -> Result<()>
      pub fn clear_scene(&self) -> Result<()>
  }
  ```

  - [ ] Add scene storage to World
  - [ ] Add mesh handle type (MeshId)
  - [ ] Implement add/remove/clear methods
  - [ ] Pass scene to SpatialProcessor on creation

- [ ] **1.5 Update SpatialProcessor to use scene**
  - [ ] Store scene reference in SpatialProcessor
  - [ ] Call `scene.commit()` after mesh changes
  - [ ] Update simulator with new scene

- [ ] **1.6 Add tests and examples**
  - [ ] Unit tests for StaticMeshBuilder
  - [ ] Integration test for scene management
  - [ ] Add example to demo showing room with walls

**Files to create:**

- `petalsonic-core/src/scene/mod.rs`
- `petalsonic-core/src/scene/geometry.rs`
- `petalsonic-core/src/scene/material.rs`
- `petalsonic-core/src/scene/builder.rs`

**Files to modify:**

- `petalsonic-core/src/lib.rs`
- `petalsonic-core/src/world.rs`
- `petalsonic-core/src/spatial/processor.rs`

**Estimated time:** 2-3 days

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

- [ ] **5.1 Add scene preset methods**

  ```rust
  impl PetalSonicWorld {
      pub fn create_simple_room(&self, width: f32, height: f32, depth: f32, material: Material) -> Result<Vec<MeshId>>
      pub fn create_hallway(&self, length: f32, width: f32, height: f32) -> Result<Vec<MeshId>>
      pub fn create_preset_hall(&self) -> Result<Vec<MeshId>>
      pub fn create_preset_cathedral(&self) -> Result<Vec<MeshId>>
  }
  ```

  - [ ] Implement simple_room helper
  - [ ] Implement hallway helper
  - [ ] Implement preset acoustic environments
  - [ ] Return mesh IDs for later removal

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

### Scene Geometry Updates

**Decision:** Immutable scene (rebuild on change)
**Rationale:**

- Simpler implementation
- Most use cases have static geometry
- Can add dynamic updates later if needed
- Steam Audio scene commit is required anyway

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
| Phase 1: Scene Geometry | 🔴 Not Started | 0% | 2-3 days | Foundation for reflections |
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

### Current Architecture Strengths

- ✅ Real-time safe design (lock-free ring buffer)
- ✅ Dedicated audio threads
- ✅ Pre-allocated buffers
- ✅ Supports resampling
- ✅ Framework-agnostic

### Current Limitations

- ❌ No reflections/reverb (scene is empty)
- ❌ No occlusion (sources omnidirectional)
- ❌ No directivity
- ❌ Basic spatial config (position + volume only)
- ❌ No scene geometry management

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

1. Create the scene module structure
2. Implement Material types
3. Build StaticMeshBuilder with primitives
4. Integrate with World and SpatialProcessor
5. Test with simple room example

### Testing Strategy

- Unit tests for each component
- Integration tests for end-to-end pipeline
- Audio quality verification (manual listening tests)
- Performance benchmarks at each phase
- Regression testing with existing functionality

---

## 📚 References

- [Steam Audio Documentation](https://valvesoftware.github.io/steam-audio/)
- [AudioNimbus Rust Bindings](https://github.com/MaxenceMaire/audionimbus)
- [Reference Implementation](petalsonic-core/src/reference-audio/)
- Comparison Analysis - see analysis above

---

## 🤝 Contributing

When implementing features:

1. Create a feature branch from `main`
2. Implement incrementally with tests
3. Update this TODO with progress
4. Document breaking changes
5. Add examples to demo
6. Submit PR with clear description

---

## 🔮 Future Ideas (Beyond Current Plan)

### Advanced Features (Planned for Future)

#### Core

- [ ] Device switch handling
- [ ] Per-zone reverb (room transitions)
- [ ] Baked reflection data support
- [ ] Path tracing for complex scenes
- [ ] Dynamic geometry updates

#### Demo

- [ ] Web UI with static audio source positioning
- [ ] Interactive drag-and-drop with real-time audio updates
- [ ] 3D visualization of acoustic scene
- [ ] Real-time HRTF customization

---

**Last Updated:** 2025-10-14
**Next Review:** After Phase 1 completion
