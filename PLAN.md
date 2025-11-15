# Seamless Loop Implementation Plan

## Problem Analysis

Currently, looping audio has an audible seam/gap when transitioning from the end back to the beginning. This occurs because:

1. When a source reaches the end, it stops playback (`PlayState::Stopped`)
2. The mixer detects the end on the **next iteration**
3. The mixer then calls `play_from_beginning()` to restart
4. Between these steps, there are one or more audio buffer cycles where the source outputs silence

This creates an audible gap/click at the loop point.

## Current Flow (Problematic)

```
Buffer N:   [... sample 9997, 9998, 9999] → reached_end_flag = true, state = Stopped
Buffer N+1: [silence...] → mixer detects end, calls play_from_beginning()
Buffer N+2: [sample 0, 1, 2, ...] → playback resumes
           ↑ GAP/SEAM HERE
```

## Solution: Wraparound Fill (Option 1)

Implement sample-accurate wraparound when filling buffers in `LoopMode::Infinite`. When we reach the end of the audio data while filling a buffer, immediately wrap around to frame 0 and continue filling the rest of the buffer.

### Design Flow (Seamless)

```
Buffer N: [... sample 9997, 9998, 9999, 0, 1, 2, ...] → seamless wraparound
         ↑ end of data              ↑ wrapped to start
         → reached_end_flag = true for event emission
         → but state remains Playing
```

## Implementation Changes

### 1. Modify `PlaybackInstance::fill_buffer()` (playback.rs)

**Current behavior:**

- Stops filling when reaching end
- Calls `advance_and_check_completion()` which sets state to Stopped

**New behavior:**

- When `LoopMode::Infinite` and reaching end:
  - Wrap current_frame back to 0
  - Continue filling the buffer
  - Set `reached_end_this_iteration = true` for event emission
  - Keep state as `Playing` (don't stop)

**Changes needed:**

```rust
pub fn fill_buffer(&mut self, buffer: &mut [f32], channels: u16) -> usize {
    if !matches!(self.info.play_state, PlayState::Playing) {
        return 0;
    }

    let channels_usize = channels as usize;
    let frame_count = buffer.len() / channels_usize;
    let samples = self.audio_data.samples();
    let total_frames = samples.len();
    let mut frames_filled = 0;
    let volume = self.config.volume();

    for frame_idx in 0..frame_count {
        let mut sample_idx = self.info.current_frame + frame_idx;

        // Handle wraparound for infinite looping
        if sample_idx >= total_frames {
            if matches!(self.loop_mode, LoopMode::Infinite) {
                // Mark that we reached end (for event emission)
                if !self.reached_end_this_iteration {
                    self.reached_end_this_iteration = true;
                }
                // Wrap around to beginning
                sample_idx = sample_idx % total_frames;
            } else {
                // LoopMode::Once - stop here
                break;
            }
        }

        let sample = samples[sample_idx];

        // Fill all channels with volume applied
        for channel in 0..channels_usize {
            let buffer_idx = frame_idx * channels_usize + channel;
            if buffer_idx < buffer.len() {
                buffer[buffer_idx] += sample * volume;
            }
        }

        frames_filled += 1;
    }

    // Advance cursor and check for completion
    if frames_filled > 0 {
        self.advance_and_check_completion_with_wrap(frames_filled);
    }

    frames_filled
}
```

### 2. Create New Method: `advance_and_check_completion_with_wrap()`

Replace the current `advance_and_check_completion()` with a version that handles wraparound:

```rust
fn advance_and_check_completion_with_wrap(&mut self, frames_consumed: usize) {
    let total_frames = self.audio_data.samples().len();
    self.info.current_frame += frames_consumed;

    // Check if we've reached or passed the end
    if self.info.current_frame >= total_frames {
        match self.loop_mode {
            LoopMode::Infinite => {
                // Wrap around - keep playing
                self.info.current_frame = self.info.current_frame % total_frames;
                // Note: reached_end_this_iteration already set in fill_buffer
                // State remains Playing
            }
            LoopMode::Once => {
                // Stop playback
                self.reached_end_this_iteration = true;
                self.info.play_state = PlayState::Stopped;
            }
        }
    }

    self.info.update_position(self.info.current_frame, self.audio_data.sample_rate());
}
```

### 3. Update Spatial Processor (spatial.rs)

The spatial processor also consumes frames directly. We need to apply the same wraparound logic there.

**Find:** The code that reads samples from `instance.audio_data.samples()` in the spatial processing loop
**Modify:** Apply the same wraparound logic when reading samples

### 4. Update Mixer Logic (mixer.rs)

**Current behavior:**

- When `LoopMode::Infinite`, calls `instance.play_from_beginning()`

**New behavior:**

- For `LoopMode::Infinite`, no longer needs to restart (already wrapped)
- Just emit the event, don't call `play_from_beginning()`

```rust
// In mixer.rs around line 136
LoopMode::Infinite => {
    // No longer need to restart - wraparound already handled in fill_buffer
    // instance.play_from_beginning(); // REMOVE THIS
    looped_sources.push(*source_id);
}
```

### 5. Update `seek()` Method

Ensure seeking still works correctly and clears the end flag when seeking:

```rust
pub fn seek(&mut self, progress: f32) {
    // ... existing seek logic ...

    // Clear end flag in case we were at the end
    self.reached_end_this_iteration = false;

    // If we were stopped due to Once mode, allow seeking to re-enable playback?
    // (Design decision - discuss if needed)
}
```

## Alternative Approaches Considered

### Option 2: Pre-restart on Last Buffer

- Detect when we're about to finish and pre-restart
- **Rejected:** Still has timing uncertainty, may loop early

### Option 3: Crossfade at Loop Point

- Add 5-20ms crossfade between end and beginning
- **Rejected:** More complex, requires extra buffering, not truly seamless

### Option 4: Double-buffered Loop Region

- Keep buffer of both end and beginning samples
- **Rejected:** Memory overhead and complex state management

## Benefits of Wraparound Approach

1. **Sample-accurate looping** - no gap between iterations
2. **Zero latency** - immediate wraparound within same buffer
3. **Works with existing architecture** - minimal changes
4. **Standard approach** - used by most audio engines (FMOD, Wwise, etc.)
5. **No memory overhead** - uses existing audio data
6. **Event emission preserved** - still fires `SourceLooped` event for tracking

## Testing Plan

1. **Basic loop test:** Play short audio file in Infinite mode, verify no audible seam
2. **Buffer boundary test:** Use audio length that doesn't align with buffer size
3. **Multiple sources test:** Ensure multiple looping sources work correctly
4. **Seek during loop test:** Seek while looping and verify it continues seamlessly
5. **Event test:** Verify `SourceLooped` events still fire correctly
6. **Spatial loop test:** Test with spatial sources to ensure wraparound works in spatial processor

## Potential Edge Cases

1. **Audio shorter than one buffer:** Need to handle multiple wraps per buffer
2. **Seek to end then loop:** Ensure wraparound still works after seeking
3. **Switch from Once to Infinite:** If stopped at end, what happens?
4. **Thread safety:** Ensure loop_mode changes are properly synchronized

## Files to Modify

1. `petalsonic/src/playback.rs` - Main fill_buffer and advance logic
2. `petalsonic/src/spatial.rs` - Spatial processor sample consumption
3. `petalsonic/src/mixer.rs` - Remove play_from_beginning call for Infinite mode
4. `petalsonic/src/playback.rs` - Update seek method if needed

## Success Criteria

- [ ] No audible gap/seam when looping in Infinite mode
- [ ] Sample-accurate loop point (verified with test audio with distinct end/start)
- [ ] `SourceLooped` events still fire correctly
- [ ] Works with both spatial and non-spatial sources
- [ ] Works with various audio lengths and buffer sizes
- [ ] No regression in Once mode behavior
