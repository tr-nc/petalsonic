# PetalSonic Implementation TODO

## Advanced Features (Planned for Future)

### Core

- [ ] Device switch handling
- [ ] Additional spatial audio features (reverb, occlusion)

### Demo

- [ ] Basic web UI with static audio source positioning
- [ ] Interactive drag-and-drop with real-time audio updates

---

## Reference Code for Steam Audio Usage

**IMPORTANT**: The `petalsonic-core/src/reference-audio/` directory contains working examples of Steam Audio integration. Study these files:

- **`spatial_sound_manager.rs`**: Complete Steam Audio setup (Context, Scene, Simulator, HRTF, effect chains)
  - See how to initialize Steam Audio objects
  - See Direct/Ambisonics effect chains
  - See per-source effect management
- **`spatial_sound.rs`**: Per-source spatial configuration
- **`pal.rs`**: Steam Audio Context/Scene/Simulator initialization patterns
