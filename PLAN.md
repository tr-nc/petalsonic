# PetalSonic realtime-safety plan (jitter when adding sources)

## Status: Short-term fix IMPLEMENTED ✓

The minimal fix described in section 3.1 has been successfully implemented:
- Playback command processing moved from audio_callback to render_thread_loop
- AudioCallbackContext cleaned up to remove unused world and active_playback references
- The audio callback is now realtime-safe: it only consumes from the ring buffer

### Changes made:
1. Added `process_playback_commands` call in `render_thread_loop` (before `generate_samples`)
2. Removed `process_playback_commands` call from `audio_callback`
3. Removed `active_playback` and `world` from `AudioCallbackContext` struct
4. Updated documentation to reflect the new realtime-safe design

## 1. Problem statement

When adding sources to the world on the fly (e.g. via the demo GUI), audible
glitches / jitter occur in the currently playing audio. This points to one or
both of:

- the realtime audio callback doing work that can block or be delayed, and/or
- the render thread failing to keep the ring buffer sufficiently full when the
  world is modified and new sources are added.

From the current code:

- `PetalSonicWorld` owns `audio_data_storage` and `source_configs` behind
  `std::sync::Mutex`.
- `PetalSonicEngine::audio_callback` calls
  `Self::process_playback_commands(&ctx.world, &ctx.active_playback);`.
- `process_playback_commands`:
  - acquires `active_playback` via `try_lock()` (good),
  - then, for `PlaybackCommand::Play`, calls `world.get_audio_data(audio_id)`,
  - which does a **blocking** `Mutex::lock()` on `audio_data_storage`.
- The demo GUI adds sources by calling `world.register_audio(...); world.play(...)`
  from the main/UI thread.
- The render thread uses `world.listener()` (blocking lock on `listener`) and
  `mixer::mix_playback_instances` with `active_playback`.

So today the realtime callback thread can block on a `Mutex` that is shared
with non-realtime code (world registration, deletion, etc.) exactly when new
sources are being added – explaining the jitter observed while adding sources.

## 2. Goals

1. Make the audio callback **hard realtime friendly**:
   - no blocking locks,
   - no calls into `PetalSonicWorld`,
   - no heap allocation or heavy work.
2. Ensure adding/removing sources or updating configs from the main thread does
   not cause glitches in already playing audio.
3. Keep the render thread design (ring buffer producer) but ensure it can
   absorb jitter from world / GUI interactions.
4. Maintain current public APIs (`PetalSonicWorld`, `PetalSonicEngine`) and
   behaviour as much as possible.

## 3. Short-term fix (minimal structural change)

### 3.1 Move playback command processing off the audio callback

Today:

- `audio_callback`:
  - checks `is_running`,
  - calls `process_playback_commands(&ctx.world, &ctx.active_playback)`,
  - consumes frames from ring buffer.
- `render_thread_loop`:
  - updates listener pose in spatial processor (`world.listener()`),
  - checks ring buffer fill level,
  - calls `generate_samples(...)` to mix/resample and push to ring buffer.

Plan:

1. Move the call to `process_playback_commands` from the audio callback into
   the render thread loop, *before* generating samples.
   - Add a call in `render_thread_loop` after computing `should_generate` and
     before `generate_samples(...)`, passing `ctx.world` and
     `ctx.active_playback`.
   - Remove the call from `audio_callback` entirely.
2. Keep the logic of `process_playback_commands` as-is:
   - it already uses `active_playback.try_lock()` (non-blocking from the
     render thread’s perspective),
   - it can safely call `world.get_audio_data(...)` because the render thread
     is not realtime-critical; if it blocks briefly, the ring buffer should
     have enough headroom.
3. Confirm `active_playback` is only used on render and audio threads:
   - after removing the call in `audio_callback`, the only remaining usages
     should be:
     - render thread (`render_thread_loop` / `generate_samples`),
     - mixer module (called from render thread).
   - This means all mutation of `active_playback` happens on the render thread,
     under the `Mutex` that it already owns, and the audio callback never
     touches it.
4. Verify that `audio_callback` becomes effectively:
   - check `is_running`,
   - compute `device_frames`,
   - `try_pop` from `ring_buffer_consumer` into output buffer,
   - fill remaining with silence if underrun,
   - update `frames_processed`.

Impact:

- Removes `PetalSonicWorld` access and blocking locks from the realtime
  callback.
- All world / playback command logic moves to the render thread, which is
  decoupled by the ring buffer.
- Existing public API remains unchanged.

### 3.2 Slightly increase robustness of render thread loop

While touching the render thread:

1. Ensure `render_thread_loop` can skip work if it fails to acquire locks:
   - `generate_samples` already uses `try_lock` on `resampler` and
     `spatial_processor`; it logs and returns if it cannot acquire locks.
   - `mixer::mix_playback_instances` uses `try_lock` on `active_playback` and
     returns zero frames if it fails.
2. Make sure `process_playback_commands` is tolerant of lock contention:
   - it already returns early if `active_playback.try_lock()` fails, leaving
     commands queued for the next iteration.
3. Optionally tune ring buffer size / target fill level:
   - keep `RING_BUFFER_SIZE_MIN` large enough (currently 100k frames) to
     survive brief render thread stalls when adding many sources.
   - consider raising `target_buffer_fill` (currently `block_size * 4`) if we
     see underruns in profiling when adding lots of sources.

Short-term success criteria:

- No audible glitches when adding a small number of sources at runtime.
- Under high load (many sources added rapidly), jitter is much reduced and
  correlates only with ring buffer underruns, not with world locks in the
  audio callback.

## 4. Medium-term design: decouple engine from world locks

Once the minimal fix is in and validated, we can further harden the design by
reducing how often the render thread touches `PetalSonicWorld` at all.

### 4.1 Stop looking up audio data via `world.get_audio_data` on Play

Current flow for `PlaybackCommand::Play`:

- world side:
  - `register_audio`:
    - resamples if needed (no lock),
    - inserts `Arc<PetalSonicAudioData>` into `audio_data_storage` under
      `SourceId`.
  - `play(source_id, loop_mode)`:
    - validates `contains_audio`,
    - looks up `SourceConfig`,
    - sends `PlaybackCommand::Play(source_id, config, loop_mode)` over channel.
- engine side:
  - `process_playback_commands` receives `Play` and calls
    `world.get_audio_data(audio_id)` to get `Arc<PetalSonicAudioData>`.

This forces the engine to depend on the world’s internal locks.

Plan:

1. Extend `PlaybackCommand::Play` to carry `Arc<PetalSonicAudioData>`:
   - e.g. `Play(SourceId, Arc<PetalSonicAudioData>, SourceConfig, LoopMode)`.
2. Change `PetalSonicWorld::play` to:
   - look up `Arc<PetalSonicAudioData>` from `audio_data_storage` on the
     **world thread** (where blocking is fine),
   - send a `Play` command that already includes the `Arc`, so the engine does
     not call back into `world.get_audio_data`.
3. Update `process_playback_commands` to drop the `world` dependency:
   - it will only need `active_playback` and the command payload.
4. After this, the render thread no longer needs `PetalSonicWorld` at all for
   playback control; only for listener pose updates in the spatial processor.

Benefits:

- Playback command handling is completely decoupled from world storage locks.
- Engine can eventually be reused with different front-ends, as it only
  depends on a command stream and not directly on the world object.

### 4.2 Decouple listener pose updates from world lock

Currently `render_thread_loop` calls `ctx.world.listener().pose()` every
iteration, which locks `listener` under `Mutex`. This is not realtime-critical
but still couples the render thread to the world’s locking behaviour.

Plan:

1. Introduce a small `Arc<Mutex<Pose>>` or `Arc<Atomic*`-backed representation
   owned by the engine for the listener pose.
2. Expose a method on `PetalSonicEngine` to set the listener pose, which can
   be called whenever `PetalSonicWorld::set_listener_pose` is called (or vice
   versa).
3. In `render_thread_loop`, read from this engine-owned pose representation,
   eliminating the need to lock `PetalSonicWorld` from the render thread.

This is optional but moves us toward a cleaner separation: the world is a
front-end; the engine is the realtime core.

## 5. Long-term enhancements (optional)

These are nice-to-haves once the core jitter issue is solved:

1. Introduce an explicit engine command API:
   - treat `PlaybackCommand` as a public or semi-public type exposed by the
     engine, with the world acting as one producer.
   - allow other producers (e.g. games, tools) to talk to the engine without
     going through `PetalSonicWorld`.
2. Make `active_playback` single-threaded:
   - move it behind a structure that is only ever accessed from the render
     thread, avoiding `Mutex` entirely for that map.
3. Finer-grained profiling of render vs spatial vs resampling:
   - `RenderTimingEvent` already carries timing fields; we can extend the
     mixer and spatial processor to report their own contributions.
   - use the existing GUI profiling panel to visualise worst-case spikes when
     adding/removing sources.

## 6. Testing and validation plan

1. Add a regression test scenario in the demo:
   - script or manual steps: start a looping spatial source, then repeatedly
     add new sources at random positions while listening for glitches.
2. Enable `log::debug!` for engine and mixer modules:
   - confirm that, after the short-term fix, the audio callback logs no
     messages involving `PetalSonicWorld` or `Mutex` locks.
3. Monitor underrun logs (`Ring buffer underrun`) while stress-adding sources:
   - expect underruns to be rare unless we deliberately overload the CPU.
4. Verify behaviour across devices / sample rates:
   - especially devices where `device_sample_rate != world.sample_rate`
     (resampler is active and render thread does more work).

Once the short-term fix is implemented and validated, we can revisit the
medium-term design steps and decide how much decoupling we want in the first
release that addresses this jitter issue.
