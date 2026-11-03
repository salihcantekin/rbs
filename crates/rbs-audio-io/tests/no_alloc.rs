//! The real-time ring calls neither allocate nor free (performance budgets).

use rbs_audio_io::edge::EdgeClock;
use rbs_audio_io::ring::{self, I24};
use rbs_core::media::AudioFormat;
use rbs_core::time::Timestamp;

#[test]
fn push_read_and_edge_do_not_allocate() {
    let (mut producer, mut consumer) = ring::channel(AudioFormat::STEREO_48K, 1_000, 4);
    let interleaved = [0.5_f32; 960];
    let packed = [I24([1, 2, 3]); 200];
    let planar = [0_i16; 300];
    let mut left = [0.0_f32; 480];
    let mut right = [0.0_f32; 480];
    let mut edge = EdgeClock::default();
    let info = allocation_counter::measure(|| {
        for k in 0..20_u64 {
            edge.observe(k * 10_000_000 + 7, Timestamp::from_nanos(k * 10_000_000));
            let t = edge.to_master(
                k * 10_000_000,
                std::time::Duration::from_millis(10),
                Timestamp::from_nanos(k * 10_000_000),
            );
            let _ = producer.push(t, &interleaved);
            let _ = producer.push(t, &packed);
            let _ = producer.push_planar(t, &[&planar, &planar]);
            let _ = producer.push_silence(t, 10);
            producer.gap();
            producer.lost(3);
            while consumer
                .read(&mut [&mut left, &mut right])
                .map(|r| r.frames)
                != Ok(0)
            {}
            let _ = consumer.available();
            let _ = consumer.stats();
        }
    });
    assert_eq!(
        info.count_total + info.count_current.unsigned_abs(),
        0,
        "{info:?}"
    );
}
