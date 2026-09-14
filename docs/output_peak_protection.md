# Output overload and near-field cicada distortion

## Audited chain

PCM → event gain → effective master/category bus gain (applied once) →
distance/air attenuation → HRTF and environmental mix → device-rate resampling →
fixed -6 dB headroom → final output protection → device callback.

The previous final protection independently clamped each sample to [-1, 1].
The Re: Flora saved mix (master 17, cicadas 128, input +1 dB, event -20/-24 dB)
can overload this boundary even with a single stationary source and no reflection.
The HRTF renderer and output callback do not contain another unintended gain
multiplier. Distance attenuation is capped at unity for distances <= 1 acoustic
metre. Near-field saturation does not require division by zero or camera motion.

The system sink was observed at 120% (+4.75 dB) on 2026-09-15. That observation
does not calibrate perceived/acoustic loudness, and no system setting was changed.
Downstream amplification can still overload a device after library protection.

## Repair

A stereo-linked sample-peak limiter now owns the final boundary after resampling.
It uses 5 ms fixed input lookahead, a bounded monotonic maximum queue, a finite
linear attack and 80 ms release toward unity. Both ears share one gain so overload
does not corrupt interaural level differences. Ceiling is 0.98; normal signals
remain unchanged except the delay. The historical -6 dB headroom remains.

The limiter is allocated when the physical-output session is connected, uses
bounded preallocated storage during rendering, and is recreated on reconnect at
the actual device rate. It applies to spatial and non-spatial output equally.
No game-specific source type, saved-slider rewrite or per-asset attenuation was
added to the backend. Source progress/completion remains render-timeline based;
the output delay is additional to existing ring/device buffering.

This is sample-peak protection, not an oversampled true-peak guarantee, loudness
normalization or a promise of distortion-free arbitrary gain. Strong sustained
overload necessarily reduces dynamics and can cause audible mix ducking. It does
not guarantee safety after external gain amplification.

## Evidence

- Red test: [3.0, 0.3] became [1.0, 0.3], breaking a 10:1 stereo ratio.
  The protected output preserves the ratio. A device-output regression also
  verifies an overloaded voice through actual 48 kHz → 44.1 kHz resampling.
- Unit coverage: unchanged normal PCM plus exact delay; overloaded sine shape
  and stereo ratio; chunk independence at 44.1/48/96 kHz; bounded queue capacity;
  malformed-sample containment.
- Real-game WAV + real HRTF, fixed direction and single direct voice:
  Dog Day peak 3.09675 → 0.98, hard-clipped samples 101507 → 0;
  Linne peak 2.12086 → 0.98, 8401 → 0.
  At 15 acoustic metres the respective peaks remain 0.20587 and 0.14099.
- This is controlled offline DSP evidence, not a recording of the user's entire
  scene or subjective listening acceptance.
- fmt/check/strict clippy; 189 unit + 6 integration + 2 doc tests passed.
  Nine expensive tests are ignored in the normal suite.
- Release realtime gate first measured 108 us against a 107 us baseline limit;
  three following isolated runs passed. Do not erase the first failure or treat
  this as a broad performance guarantee.
- Re: Flora local override: 979+4 tests passed. First hidden/muted run blocked
  while ALSA/PipeWire opened a physical stream; stack capture showed
  snd_pcm_pipewire_prepare / OutputPlatform::open, not DSP. That test process was
  terminated after capture. Retry completed with failures=0:
  target/re-flora-logs/re-flora-20260915-004933.592-55823.log.
  The platform-open incident is not proven fixed or baseline-only.
  Stack evidence: /tmp/output-limiter-shutdown-stacks.log.

Reproduce the opt-in real-asset probe:

```sh
PETALSONIC_CICADA_ASSET_ROOT=/path/to/re-flora/assets \
  cargo test --release -p petalsonic cicada_assets_do_not_hard_clip_at_near_distance -- --ignored --nocapture
```
