# Capture spatial routing per Voice

Direct Path and Environment Send routing are copied into each Voice when play is accepted. In particular, a fixed acoustic origin is never stored only on a reusable Emitter: overlapping plays may share the Emitter and Resident Clip, but later plays must not relocate earlier environment responses. The compatibility default remains a world-space Direct Path and an Environment Send that follows the Emitter.

The Voice owns one PCM cursor. Rendering fills one mono block and fans that block into Direct Path
and Environment Send processing; a route never creates a second playback or decoder. Direct
placement, geometry transmission, and propagation timing remain separate policies.

When PCM completes, direct state retires immediately. Active per-Voice early reflections drain to
a bounded release threshold, while the shared late response drains independently. This preserves
environment tails without extending or replaying the Voice cursor.
