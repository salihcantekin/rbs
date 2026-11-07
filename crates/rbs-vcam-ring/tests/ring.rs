//! Ring protocol tests (spec `docs/specs/virtual-camera.md`, "Conformance tests").
//! Deterministic: time is passed in, interleavings come from a seeded generator.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // Tests fail by panicking.

use rbs_vcam_ring::{
    FrameInfo, Layout, NoFrames, Reader, Region, RingError, SLOTS, STALE_MS, WriteOutcome, Writer,
    initialize,
};

const W: u32 = 16;
const H: u32 = 8;

fn layout() -> Layout {
    Layout::nv12(W, H, W).unwrap()
}

/// A frame whose every byte encodes `seq`, so a torn copy shows.
fn frame(layout: &Layout, seq: u64) -> Vec<u8> {
    vec![(seq % 251) as u8 + 1; layout.frame_len()]
}

fn is_frame_of(buf: &[u8], seq: u64) -> bool {
    let b = (seq % 251) as u8 + 1;
    buf.iter().all(|&x| x == b)
}

fn ring(generation: u32) -> (Region, Layout) {
    let l = layout();
    let r = Region::owned(l.total_len());
    initialize(&r, &l, generation).unwrap();
    (r, l)
}

/// xorshift64*: a deterministic generator for interleavings.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn layouts_are_validated() {
    assert_eq!(Layout::nv12(0, 2, 2), Err(RingError::InvalidLayout));
    assert_eq!(Layout::nv12(3, 2, 4), Err(RingError::InvalidLayout));
    assert_eq!(Layout::nv12(4, 2, 2), Err(RingError::InvalidLayout));
    let l = Layout::nv12(1280, 720, 1280).unwrap();
    assert_eq!(l.frame_len(), 1280 * 720 * 3 / 2);
    assert!(l.total_len() >= l.frame_len() * SLOTS);
}

#[test]
fn a_written_frame_is_read_once() {
    let (r, l) = ring(1);
    let mut w = Writer::attach(r.clone(), &l).unwrap();
    let mut rd = Reader::new(r, l).unwrap();
    let mut out = vec![0; l.frame_len()];
    assert_eq!(
        w.write(10, 42, &frame(&l, 1)).unwrap(),
        WriteOutcome::Written { seq: 1 }
    );
    let info = rd.read_newest(10, &mut out).unwrap().unwrap();
    assert_eq!(
        info,
        FrameInfo {
            seq: 1,
            timestamp_ns: 42
        }
    );
    assert!(is_frame_of(&out, 1));
    assert_eq!(rd.read_newest(10, &mut out).unwrap(), None);
}

#[test]
fn the_newest_frame_wins_and_sequence_numbers_increase() {
    let (r, l) = ring(1);
    let mut w = Writer::attach(r.clone(), &l).unwrap();
    let mut rd = Reader::new(r, l).unwrap();
    let mut out = vec![0; l.frame_len()];
    let mut last = 0;
    for i in 1..=10u64 {
        match w.write(i, i as i64, &frame(&l, i)).unwrap() {
            WriteOutcome::Written { seq } => {
                assert!(seq > last);
                last = seq;
            }
            other => panic!("{other:?}"),
        }
    }
    let info = rd.read_newest(10, &mut out).unwrap().unwrap();
    assert_eq!(info.seq, last);
    assert!(is_frame_of(&out, last));
}

#[test]
fn the_writer_never_waits_for_a_held_slot() {
    let (r, l) = ring(1);
    let mut w = Writer::attach(r.clone(), &l).unwrap();
    let mut rd = Reader::new(r, l).unwrap();
    w.write(0, 0, &frame(&l, 1)).unwrap();
    let guard = rd.begin(0).unwrap().unwrap();
    // One slot held: the writer cycles through the other two.
    for i in 2..50u64 {
        assert!(matches!(
            w.write(1, 0, &frame(&l, i)).unwrap(),
            WriteOutcome::Written { .. }
        ));
    }
    assert!(guard.finish());
}

#[test]
fn a_stopped_readers_slot_is_taken_back_and_its_copy_discarded() {
    let (r, l) = ring(1);
    let mut w = Writer::attach(r.clone(), &l).unwrap();
    let mut rd = Reader::new(r.clone(), l).unwrap();
    // Every slot held by readers (as a second, broken reader would leave them).
    for i in 1..=SLOTS as u64 {
        w.write(0, 0, &frame(&l, i)).unwrap();
    }
    let mut readers: Vec<Reader> = (0..SLOTS)
        .map(|_| Reader::new(r.clone(), l).unwrap())
        .collect();
    let guards: Vec<_> = readers
        .iter_mut()
        .map(|rd| {
            // Each takes the newest ready slot not yet taken.
            rd.begin(0).unwrap().unwrap()
        })
        .collect();
    assert_eq!(
        w.write(STALE_MS, 0, &frame(&l, 9)).unwrap(),
        WriteOutcome::Busy
    );
    // The readers stop: their heartbeat ages past STALE_MS.
    assert!(matches!(
        w.write(STALE_MS + 1, 0, &frame(&l, SLOTS as u64 + 1))
            .unwrap(),
        WriteOutcome::Written { .. }
    ));
    let finished: Vec<bool> = guards.into_iter().map(|g| g.finish()).collect();
    assert_eq!(finished.iter().filter(|ok| !**ok).count(), 1);
    let mut out = vec![0; l.frame_len()];
    let info = rd.read_newest(STALE_MS + 1, &mut out).unwrap().unwrap();
    assert!(is_frame_of(&out, info.seq));
}

#[test]
fn a_new_generation_resets_the_writer_and_the_reader() {
    let (r, l) = ring(1);
    let mut w = Writer::attach(r.clone(), &l).unwrap();
    let mut rd = Reader::new(r.clone(), l).unwrap();
    let mut out = vec![0; l.frame_len()];
    for i in 1..=5 {
        w.write(0, 0, &frame(&l, i)).unwrap();
    }
    rd.read_newest(0, &mut out).unwrap().unwrap();
    initialize(&r, &l, 2).unwrap();
    assert_eq!(w.write(0, 0, &frame(&l, 6)).unwrap(), WriteOutcome::Reset);
    let mut w = Writer::attach(r, &l).unwrap();
    let WriteOutcome::Written { seq } = w.write(0, 0, &frame(&l, 7)).unwrap() else {
        panic!()
    };
    // Low sequence numbers of the new generation are still read.
    let info = rd.read_newest(0, &mut out).unwrap().unwrap();
    assert_eq!(info.seq, seq);
}

#[test]
fn a_writer_that_stopped_mid_frame_frees_its_slot_on_attach() {
    let (r, l) = ring(1);
    let mut w = Writer::attach(r.clone(), &l).unwrap();
    for i in 1..=SLOTS as u64 {
        w.write(0, 0, &frame(&l, i)).unwrap();
    }
    drop(w);
    let mut w = Writer::attach(r, &l).unwrap();
    assert!(matches!(
        w.write(0, 0, &frame(&l, 1)).unwrap(),
        WriteOutcome::Written { .. }
    ));
}

#[test]
fn the_reader_sees_no_frames_when_the_writer_stops_or_disagrees() {
    let (r, l) = ring(1);
    let mut w = Writer::attach(r.clone(), &l).unwrap();
    w.write(100, 0, &frame(&l, 1)).unwrap();
    let rd = Reader::new(r.clone(), l).unwrap();
    assert_eq!(rd.status(100 + STALE_MS), Ok(()));
    assert_eq!(rd.status(101 + STALE_MS), Err(NoFrames::WriterStopped));
    let other = Layout::nv12(W * 2, H, W * 2).unwrap();
    let big = Region::owned(other.total_len());
    initialize(&big, &l, 1).unwrap();
    let mut rd = Reader::new(big, other).unwrap();
    assert_eq!(
        rd.begin(0).err(),
        Some(NoFrames::Header(RingError::FormatMismatch))
    );
    assert_eq!(
        Writer::attach(Region::owned(l.total_len()), &l).err(),
        Some(RingError::NotInitialized)
    );
}

#[test]
fn a_frame_of_the_wrong_size_is_refused() {
    let (r, l) = ring(1);
    let mut w = Writer::attach(r, &l).unwrap();
    assert_eq!(w.write(0, 0, &[0; 3]), Err(RingError::InvalidFrame));
}

#[test]
fn small_regions_are_refused() {
    let l = layout();
    let r = Region::owned(l.total_len() - 64);
    assert_eq!(initialize(&r, &l, 1), Err(RingError::BadRegion));
    assert!(Reader::new(r, l).is_err());
}

/// Any interleaving of one writer and one reader, with the reader's copy split from its
/// claim and finish: every accepted copy is whole, sequence numbers read increase.
#[test]
fn interleavings_never_yield_a_torn_frame() {
    let l = layout();
    for seed in 1..=200u64 {
        let r = Region::owned(l.total_len());
        initialize(&r, &l, 1).unwrap();
        let mut w = Writer::attach(r.clone(), &l).unwrap();
        let mut rd = Reader::new(r.clone(), l).unwrap();
        let mut rng = Rng(seed);
        let mut now = 0u64;
        let mut next = 1u64;
        let mut last_read = 0u64;
        let mut out = vec![0; l.frame_len()];
        for _ in 0..300 {
            now += rng.below(700);
            match rng.below(3) {
                0 => {
                    if let WriteOutcome::Written { .. } = w.write(now, 0, &frame(&l, next)).unwrap()
                    {
                        next += 1;
                    }
                }
                _ => {
                    let Ok(Some(mut g)) = rd.begin(now) else {
                        continue;
                    };
                    let seq = g.info().seq;
                    // Writes may overlap the copy.
                    for _ in 0..rng.below(4) {
                        now += rng.below(700);
                        if let WriteOutcome::Written { .. } =
                            w.write(now, 0, &frame(&l, next)).unwrap()
                        {
                            next += 1;
                        }
                    }
                    g.copy_to(&mut out);
                    if rng.below(5) == 0 {
                        drop(g);
                        continue;
                    }
                    if g.finish() {
                        assert!(is_frame_of(&out, seq), "seed {seed}: torn frame {seq}");
                        assert!(seq > last_read);
                        last_read = seq;
                    }
                }
            }
        }
    }
}

/// Arbitrary values in any shared word (what a hostile peer can write) never make the
/// reader panic or read outside its own layout, and never make the writer panic.
#[test]
fn a_corrupted_region_never_breaks_either_side() {
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicU64, Ordering};
    let l = layout();
    let mut rng = Rng(7);
    let mut out = vec![0; l.frame_len()];
    let words: Box<[AtomicU64]> = (0..l.total_len() / 8).map(|_| AtomicU64::new(0)).collect();
    // The 8-byte words of the header and the three slot headers.
    let slot_stride = (l.total_len() - 128) / SLOTS / 8;
    let shared: Vec<usize> = (0..16)
        .chain((0..SLOTS).flat_map(|s| (0..8).map(move |i| 16 + s * slot_stride + i)))
        .collect();
    for _ in 0..5000 {
        // SAFETY: `words` outlives every region made here (dropped at the end of the test)
        // and is only accessed through atomics or the ring.
        let r = unsafe { Region::from_raw(NonNull::from(&*words).cast(), l.total_len()) }.unwrap();
        initialize(&r, &l, rng.next() as u32).unwrap();
        let mut w = Writer::attach(r.clone(), &l).unwrap();
        let mut rd = Reader::new(r, l).unwrap();
        w.write(0, 0, &frame(&l, 1)).unwrap();
        for _ in 0..1 + rng.below(6) {
            let i = shared[rng.below(shared.len() as u64) as usize];
            let v = match rng.below(4) {
                0 => rng.next(),
                1 => rng.below(16),
                2 => (rng.below(1 << 20) << 2) | rng.below(4),
                _ => u64::MAX,
            };
            words[i].store(v, Ordering::Relaxed);
        }
        let now = rng.below(3000);
        let _ = rd.read_newest(now, &mut out);
        let _ = rd.status(now);
        let _ = w.write(now, 0, &frame(&l, 2));
        let _ = rd.read_newest(now, &mut out);
    }
}

#[test]
fn threads_never_see_a_torn_frame() {
    let (r, l) = ring(1);
    let mut w = Writer::attach(r.clone(), &l).unwrap();
    let mut rd = Reader::new(r, l).unwrap();
    // Both sides use the same time, so the writer never takes a slot back from this
    // reader: only the honest protocol runs.
    let writer = std::thread::spawn(move || {
        for i in 1..=2000u64 {
            let _ = w.write(0, 0, &frame(&l, i));
        }
    });
    let mut out = vec![0; l.frame_len()];
    let mut read = 0;
    // Bounded: a hang guard, not a timing assumption.
    for t in 0..200_000u64 {
        if let Ok(Some(info)) = rd.read_newest(0, &mut out) {
            assert!(is_frame_of(&out, info.seq));
            read += 1;
        }
        if writer.is_finished() && t > 1000 {
            break;
        }
    }
    writer.join().unwrap();
    assert!(read > 0);
}
