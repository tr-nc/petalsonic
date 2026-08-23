# PetalSonic Audio Context

PetalSonic owns playback progress and turns immutable resident audio into bounded direct and environmental spatial responses.

## Language

**Emitter**:
A reusable logical sound origin and its low-frequency defaults. It is not an individual playback or acoustic event.

**Voice**:
One playback of a Resident Clip with its own cursor, immutable spatial routing, captured source
extent and occlusion policy, and lifecycle.
_Avoid_: Source, playback instance

**Source Extent**:
The finite local domain over which one Voice's source power is distributed. It is Point or bounded
stable weighted representatives, never a collection of playback Voices.

**Occlusion Profile**:
The policy that converts sampled material transmission into direct/environment gains and optional
stable classification. It is independent of Source Extent and route placement.

**Direction Field**:
The bounded set of energy-normalized direct lobes rendered from one extended Voice and one decoded
PCM block.

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
One complete, versioned listener and dynamic-emitter pose/extent snapshot consumed atomically by
rendering and acoustics.

**Acoustic Origin**:
The world-space origin captured by an Environment Send. A fixed origin belongs to a Voice, not its reusable Emitter.

**Direct Placement**:
The coordinate semantics of the Direct Path: world, listener-relative, or disabled. It does not
decide geometry transmission or propagation timing.

**Direct Geometry**:
The Direct Path's obstruction/transmission policy. It is independent of Direct Placement.

**Direct Propagation**:
The Direct Path's timing policy. `Immediate` is the currently supported model.

**Play Command ID**:
A caller-owned correlation value used only when per-Voice render telemetry is requested.

**Voice Route Generation**:
A worker-owned generation assigned whenever a Voice identity is activated or its captured routing
is replaced. It is independent of ordinary pose revisions and prevents completed work from being
applied to a retired or rerouted Voice.

**Acoustic Publication Compatibility**:
The rule for safely publishing completed asynchronous work: the acoustic scene version must still
match, and each per-Voice result must still match its Voice identity and route generation. A newer
listener/emitter pose revision makes a response spatially older but does not by itself make it
incompatible.
