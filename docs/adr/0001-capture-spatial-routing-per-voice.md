# Capture spatial routing per Voice

Direct Path and Environment Send routing are copied into each Voice when play is accepted. In particular, a fixed acoustic origin is never stored only on a reusable Emitter: overlapping plays may share the Emitter and Resident Clip, but later plays must not relocate earlier environment responses. The compatibility default remains a world-space Direct Path and an Environment Send that follows the Emitter.
