use petalsonic::{
    EmitterDesc, OutputDevicePolicy, PetalSonicError, PetalSonicEvent, PetalSonicWorld,
    PetalSonicWorldDesc, PlayOptions, PlaybackTag, ResidentClip, RuntimeState,
};
use std::time::{Duration, Instant};

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

#[test]
fn world_interface_observes_automatic_recovery_progress_and_terminal_close() {
    let world = PetalSonicWorld::new(recovering_world_desc()).unwrap();
    let emitter = world
        .create_emitter(short_clip(), EmitterDesc::non_spatial())
        .unwrap();
    world
        .play_controlled(emitter, PlayOptions::once(), PlaybackTag(41))
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut completed = false;
    while Instant::now() < deadline {
        completed |= world.drain_events().into_iter().any(|event| {
            matches!(
                event,
                PetalSonicEvent::PlaybackCompleted {
                    tag: PlaybackTag(41),
                    ..
                }
            )
        });
        if completed && world.active_voice_count() == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(
        completed,
        "the World-owned runtime must advance without a pump"
    );
    assert_eq!(world.active_voice_count(), 0);
    assert_eq!(world.runtime_status().state, RuntimeState::Recovering);
    assert!(world.runtime_status().recovery_attempts > 0);

    world.close().unwrap();
    world.close().unwrap();
    assert_eq!(world.runtime_status().state, RuntimeState::Closed);
    assert!(matches!(
        world.play(emitter, PlayOptions::once()),
        Err(PetalSonicError::RuntimeClosed)
    ));
}

#[test]
fn closing_one_world_does_not_close_another_world_runtime() {
    let first = PetalSonicWorld::new(recovering_world_desc()).unwrap();
    let second = PetalSonicWorld::new(recovering_world_desc()).unwrap();
    let first_emitter = first
        .create_emitter(short_clip(), EmitterDesc::non_spatial())
        .unwrap();
    let second_emitter = second
        .create_emitter(short_clip(), EmitterDesc::non_spatial())
        .unwrap();

    first.close().unwrap();
    assert_eq!(first.runtime_status().state, RuntimeState::Closed);
    assert!(matches!(
        first.play(first_emitter, PlayOptions::once()),
        Err(PetalSonicError::RuntimeClosed)
    ));

    second
        .play(second_emitter, PlayOptions::once())
        .expect("closing one World must not affect another runtime");
    assert_ne!(second.runtime_status().state, RuntimeState::Closed);
    second.close().unwrap();
}
