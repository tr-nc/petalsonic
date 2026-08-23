# PetalSonic Audio Context

PetalSonic owns playback progress and turns immutable resident audio into bounded direct and environmental spatial responses.

## Language

**Emitter**:
A reusable logical sound origin and its low-frequency defaults. It is not an individual playback or acoustic event.

**Voice**:
One playback of a Resident Clip with its own cursor, immutable spatial routing, and lifecycle.
_Avoid_: Source, playback instance

**Direct Path**:
The audible source-to-listener contribution, with independent placement, geometry, and propagation semantics.
_Avoid_: Dry path

**Environment Send**:
The contribution from one Voice cursor routed toward early and late environmental responses from a captured acoustic origin.
_Avoid_: Wet path, reverb send

**Environment Response**:
The bounded early-reflection and late-tail contribution produced from Environment Sends and an immutable acoustic scene.
_Avoid_: Wet signal

**Spatial Frame**:
One complete, versioned listener and dynamic-emitter world snapshot consumed atomically by rendering and acoustics.

**Acoustic Origin**:
The world-space origin captured by an Environment Send. A fixed origin belongs to a Voice, not its reusable Emitter.
