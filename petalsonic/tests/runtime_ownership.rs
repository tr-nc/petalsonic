use petalsonic::{
    EmitterDesc, OutputDevicePolicy, PetalSonicError, PetalSonicEvent, PetalSonicWorld,
    PetalSonicWorldDesc, PlayOptions, PlaybackTag, ResidentClip, RuntimeState,
};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(2);

fn recovering_world_desc() -> PetalSonicWorldDesc {
    PetalSonicWorldDesc {
        block_size: 64,
        output_device: OutputDevicePolicy::PinnedNameContains(
            "petalsonic-runtime-ownership-test-device-that-does-not-exist".into(),
        ),
        ..Default::default()
    }
}

fn short_clip() -> ResidentClip {
    ResidentClip::from_mono_pcm(vec![0.25; 16], 48_000).unwrap()
}

fn wait_until<T>(mut observe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(value) = observe() {
            return value;
        }
        assert!(Instant::now() < deadline, "World observation timed out");
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

#[test]
fn world_owns_progress_and_reports_exact_voice_completion() {
    let world = PetalSonicWorld::new(recovering_world_desc()).unwrap();
    let emitter = world
        .create_emitter(short_clip(), EmitterDesc::non_spatial())
        .unwrap();
    let control = world
        .play_controlled(emitter, PlayOptions::once(), PlaybackTag(41))
        .unwrap();

    let completion = wait_until(|| {
        world.drain_events().into_iter().find(|event| {
            matches!(
                event,
                PetalSonicEvent::PlaybackCompleted {
                    tag: PlaybackTag(41),
                    ..
                }
            )
        })
    });

    assert_eq!(
        completion,
        PetalSonicEvent::PlaybackCompleted {
            emitter,
            control,
            tag: PlaybackTag(41),
        },
        "completion must preserve Emitter and Voice control identity"
    );
    assert_eq!(world.active_voice_count(), 0);
    assert_eq!(world.runtime_status().state, RuntimeState::Recovering);
    assert!(world.runtime_status().recovery_attempts > 0);

    assert!(matches!(
        world.pause_playback(control),
        Err(PetalSonicError::StalePlayback)
    ));
    let later = world
        .play_controlled(emitter, PlayOptions::looping(), PlaybackTag(42))
        .unwrap();
    assert_ne!(control, later);
    assert!(matches!(
        world.pause_playback(control),
        Err(PetalSonicError::StalePlayback)
    ));

    world.close().unwrap();
    world.close().unwrap();
    assert_eq!(world.runtime_status().state, RuntimeState::Closed);
    assert!(matches!(
        world.play(emitter, PlayOptions::once()),
        Err(PetalSonicError::RuntimeClosed)
    ));
}

#[test]
fn overload_is_bounded_and_lifecycle_capacity_remains_reserved() {
    let event_world = PetalSonicWorld::new(PetalSonicWorldDesc {
        max_emitters: 1,
        max_voices: 16,
        control_queue_capacity: 32,
        event_queue_capacity: 1,
        ..recovering_world_desc()
    })
    .unwrap();
    let emitter = event_world
        .create_emitter(short_clip(), EmitterDesc::non_spatial())
        .unwrap();
    assert!(matches!(
        event_world.create_emitter(short_clip(), EmitterDesc::non_spatial()),
        Err(PetalSonicError::CapacityExceeded {
            resource: "emitter",
            limit: 1
        })
    ));
    for tag in 0..16 {
        event_world
            .play_controlled(emitter, PlayOptions::once(), PlaybackTag(tag))
            .unwrap();
    }
    assert!(matches!(
        event_world.play(emitter, PlayOptions::once()),
        Err(PetalSonicError::CapacityExceeded {
            resource: "voice",
            limit: 16
        })
    ));

    wait_until(|| (event_world.diagnostics().dropped_events > 0).then_some(()));
    let diagnostics = event_world.diagnostics();
    assert_eq!(diagnostics.event_queue_high_water, 1);
    assert!(diagnostics.event_queue_depth <= 1);
    assert!(diagnostics.dropped_events > 0);
    event_world.close().unwrap();

    let pressure_world = PetalSonicWorld::new(PetalSonicWorldDesc {
        control_queue_capacity: 1,
        lifecycle_queue_capacity: 1,
        ..recovering_world_desc()
    })
    .unwrap();
    let emitter = pressure_world
        .create_emitter(short_clip(), EmitterDesc::non_spatial())
        .unwrap();
    let rejected = (0..10_000).any(|_| {
        matches!(
            pressure_world.pause_emitter(emitter),
            Err(PetalSonicError::QueuePressure)
        )
    });
    assert!(rejected, "regular control queue never reported pressure");
    pressure_world
        .stop_emitter(emitter)
        .expect("lifecycle reserve must remain independently available");
    let diagnostics = pressure_world.diagnostics();
    assert_eq!(diagnostics.control_queue_high_water, 1);
    assert_eq!(diagnostics.lifecycle_queue_high_water, 1);
    assert!(diagnostics.rejected_commands > 0);
    pressure_world.close().unwrap();
}

#[test]
fn close_and_identity_are_isolated_per_world() {
    let first = PetalSonicWorld::new(recovering_world_desc()).unwrap();
    let second = PetalSonicWorld::new(recovering_world_desc()).unwrap();
    let first_emitter = first
        .create_emitter(short_clip(), EmitterDesc::non_spatial())
        .unwrap();
    let second_emitter = second
        .create_emitter(short_clip(), EmitterDesc::non_spatial())
        .unwrap();
    let first_control = first
        .play_controlled(first_emitter, PlayOptions::looping(), PlaybackTag(1))
        .unwrap();
    let second_control = second
        .play_controlled(second_emitter, PlayOptions::looping(), PlaybackTag(2))
        .unwrap();

    assert!(matches!(
        first.pause_emitter(second_emitter),
        Err(PetalSonicError::StaleEmitter)
    ));
    assert!(matches!(
        first.pause_playback(second_control),
        Err(PetalSonicError::StalePlayback)
    ));
    first.pause_playback(first_control).unwrap();

    first.close().unwrap();
    first.close().unwrap();
    assert_eq!(first.runtime_status().state, RuntimeState::Closed);
    assert!(matches!(
        first.play(first_emitter, PlayOptions::once()),
        Err(PetalSonicError::RuntimeClosed)
    ));

    second.resume_playback(second_control).unwrap();
    second
        .play(second_emitter, PlayOptions::looping())
        .expect("closing one World must not affect another runtime");
    assert_ne!(second.runtime_status().state, RuntimeState::Closed);
    second.close().unwrap();
}
