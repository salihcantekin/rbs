//! The synthetic adapter delivers its signal without gaps, at the right times.

use std::sync::Arc;
use std::time::Duration;

use rbs_audio_io::{AudioBackend, EndpointId, InputRequest};
use rbs_audio_io_test::{
    LOOPBACK, SOURCE_OFFSET, STEREO_INPUT, TestAudioBackend, signal, to_master, to_source,
};
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_core::timing::SampleRate;

#[test]
fn samples_are_continuous_and_timestamps_follow_the_device_clock() {
    let backend = TestAudioBackend::new();
    let clock = TestClock::new();
    clock.advance(Duration::from_secs(1));
    let request = InputRequest::new(EndpointId::new(STEREO_INPUT), Arc::new(clock.clone()));
    let Ok(mut stream) = backend.open_input(&request) else {
        panic!("open failed");
    };
    assert!(stream.start().is_ok());
    let start_source = to_source(clock.now());
    let rate = SampleRate::HZ_48000;

    let mut left = vec![0.0_f32; 700];
    let mut right = vec![0.0_f32; 700];
    let mut next = 0_u64;
    // Read in odd-sized pieces over 3 s, so reads cross packets.
    for step in 0..600 {
        clock.advance(Duration::from_millis(5));
        loop {
            let Ok(read) = stream.read(&mut [&mut left, &mut right]) else {
                panic!("read failed");
            };
            if read.frames == 0 {
                break;
            }
            assert_eq!(read.discontinuity, next == 0, "step {step}");
            for i in 0..read.frames {
                assert_eq!(left[i], signal(next + i as u64, 0, rate), "frame {}", next);
                assert_eq!(right[i], signal(next + i as u64, 1, rate));
            }
            // The true capture time of the first frame, through the device clock.
            let source = start_source + next * 1_000_000_000 / 48_000;
            let truth = to_master(source);
            let error = read.timestamp.as_nanos().abs_diff(truth.as_nanos());
            // The edge starts with one observation (rate 1.0) and then fits the drift.
            let bound = if step < 100 { 100_000 } else { 5_000 };
            assert!(error <= bound, "step {step}: error {error} ns");
            next += read.frames as u64;
        }
    }
    // 3 s of device time at 100 ppm fast, in 10 ms packets.
    assert!(next >= 3 * 48_000 - 480, "{next}");
    assert_eq!(stream.stats().dropped, 0);
    assert!(SOURCE_OFFSET > Duration::ZERO);
}

#[test]
fn requested_rate_changes_the_signal_rate() {
    let backend = TestAudioBackend::new();
    let clock = TestClock::new();
    let request = InputRequest::new(EndpointId::new(LOOPBACK), Arc::new(clock.clone()))
        .with_sample_rates(&[SampleRate::HZ_48000]);
    let Ok(mut stream) = backend.open_input(&request) else {
        panic!("open failed");
    };
    assert_eq!(stream.format().sample_rate, SampleRate::HZ_48000);
    assert!(stream.start().is_ok());
    clock.advance(Duration::from_millis(100));
    let mut left = vec![0.0_f32; 10_000];
    let mut right = vec![0.0_f32; 10_000];
    let Ok(read) = stream.read(&mut [&mut left, &mut right]) else {
        panic!("read failed");
    };
    // Ten whole 10 ms packets at 48 kHz.
    assert_eq!(read.frames, 4_800);
    assert!(read.timestamp < Timestamp::from_nanos(100_000_000));
    assert_eq!(left[100], signal(100, 0, SampleRate::HZ_48000));
}
