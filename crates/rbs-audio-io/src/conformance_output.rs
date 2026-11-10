//! The conformance suite for audio output (`open_output`, ADR-0097).
//!
//! Separate from [`crate::conformance`] because an output has no read side: the OS
//! render callback is the consumer. The harness plays the callback's part through a
//! `pump` closure the adapter's test provides: `pump(frames)` makes the device take
//! (up to) that many frames from every started output, as its callback would. The
//! synthetic adapter does it directly; a hardware adapter's test lets real time pass
//! and reports how many frames the device asked for.

use rbs_core::capability::CapabilitySet;
use rbs_core::conformance::guarded;

use crate::caps;
use crate::{AudioBackend, AudioIoError, EndpointId, EndpointKind, OutputRequest};

pub use rbs_core::conformance::Report;

/// Runs the output suite on `endpoint` (a playback endpoint from
/// `enumerate(EndpointKind::Output)`).
pub fn run<B: AudioBackend + ?Sized>(
    backend: &B,
    caps_set: &CapabilitySet,
    endpoint: &EndpointId,
    pump: &mut dyn FnMut(usize),
) -> Report {
    let mut report = Report::default();
    report.check("output capability", capability(caps_set));
    if caps_set.get(&caps::OUTPUT).is_none() {
        report.check("output unsupported", unsupported(backend, endpoint));
        return report;
    }
    report.check("output enumerate", enumerate(backend, endpoint));
    report.check("output unknown endpoint", unknown(backend));
    report.check(
        "output standby",
        guarded(|| standby(backend, endpoint, pump)),
    );
    report.check(
        "output accounting",
        guarded(|| accounting(backend, endpoint, pump)),
    );
    report.check(
        "output overrun",
        guarded(|| overrun(backend, endpoint, pump)),
    );
    report.check(
        "output restart",
        guarded(|| restart(backend, endpoint, pump)),
    );
    report
}

fn capability(set: &CapabilitySet) -> Result<(), String> {
    let has = set.get(&caps::OUTPUT).is_some();
    let lists = set.get(&caps::OUTPUT_ENUMERATE).is_some();
    if has && !lists {
        return Err("audio.output without audio.output.enumerate".into());
    }
    Ok(())
}

fn unsupported<B: AudioBackend + ?Sized>(b: &B, id: &EndpointId) -> Result<(), String> {
    match b.open_output(&OutputRequest::new(id.clone())) {
        Err(AudioIoError::Unsupported(_)) => Ok(()),
        Err(e) => Err(format!("expected Unsupported, got {e}")),
        Ok(_) => Err("opened an output without reporting audio.output".into()),
    }
}

fn enumerate<B: AudioBackend + ?Sized>(b: &B, id: &EndpointId) -> Result<(), String> {
    let listed = b
        .enumerate(EndpointKind::Output)
        .map_err(|e| format!("enumerate(Output): {e}"))?;
    if listed.iter().any(|e| e.kind != EndpointKind::Output) {
        return Err("enumerate(Output) returned another kind".into());
    }
    if !listed.iter().any(|e| &e.id == id) {
        return Err(format!("{id} is not listed by enumerate(Output)"));
    }
    Ok(())
}

fn unknown<B: AudioBackend + ?Sized>(b: &B) -> Result<(), String> {
    let request = OutputRequest::new(EndpointId::new("org.rbs.conformance.missing-endpoint"));
    match b.open_output(&request) {
        Err(AudioIoError::EndpointNotFound) => Ok(()),
        Err(e) => Err(format!("expected EndpointNotFound, got {e}")),
        Ok(_) => Err("opened an endpoint that does not exist".into()),
    }
}

fn open<B: AudioBackend + ?Sized>(
    b: &B,
    id: &EndpointId,
    ring: usize,
) -> Result<Box<dyn crate::AudioOutput>, String> {
    b.open_output(&OutputRequest::new(id.clone()).with_ring_frames(ring))
        .map_err(|e| format!("open_output {id}: {e}"))
}

/// Opening activates nothing: no frame is played until `start`, and the producer can
/// be taken once.
fn standby<B: AudioBackend + ?Sized>(
    b: &B,
    id: &EndpointId,
    pump: &mut dyn FnMut(usize),
) -> Result<(), String> {
    let mut out = open(b, id, 1_024)?;
    let mut producer = out.producer().ok_or("no producer")?;
    if out.producer().is_some() {
        return Err("the producer can be taken twice".into());
    }
    let channels = producer.channels();
    if channels != out.format().channels.channels() {
        return Err("producer channels differ from the format".into());
    }
    producer.push(&vec![0.25; 100 * channels]);
    pump(100);
    let s = out.stats();
    if s.played != 0 {
        return Err(format!("{} frames played before start", s.played));
    }
    out.status()
        .map_err(|e| format!("status before start: {e}"))
}

/// `written == played + dropped + queued`, and a starved callback is counted as
/// silence, never as played audio.
fn accounting<B: AudioBackend + ?Sized>(
    b: &B,
    id: &EndpointId,
    pump: &mut dyn FnMut(usize),
) -> Result<(), String> {
    let mut out = open(b, id, 4_096)?;
    let mut producer = out.producer().ok_or("no producer")?;
    let channels = producer.channels();
    out.start().map_err(|e| format!("start: {e}"))?;
    producer.push(&vec![0.5; 1_000 * channels]);
    pump(600);
    let s = out.stats();
    if s.written != 1_000 || s.played != 600 || s.dropped != 0 {
        return Err(format!("after 1000 written and 600 pumped: {s:?}"));
    }
    pump(900);
    let s = out.stats();
    if s.played != 1_000 {
        return Err(format!("played {} of 1000 written frames", s.played));
    }
    if s.silence == 0 {
        return Err("an empty ring was not counted as silence".into());
    }
    out.stop();
    Ok(())
}

/// A full ring drops the newest frames whole and counts them; the producer never
/// waits.
fn overrun<B: AudioBackend + ?Sized>(
    b: &B,
    id: &EndpointId,
    pump: &mut dyn FnMut(usize),
) -> Result<(), String> {
    let mut out = open(b, id, 256)?;
    let mut producer = out.producer().ok_or("no producer")?;
    let channels = producer.channels();
    out.start().map_err(|e| format!("start: {e}"))?;
    let space = producer.space();
    let offered = space + 100;
    let stored = producer.push(&vec![0.5; offered * channels]);
    if stored != space {
        return Err(format!(
            "stored {stored} frames of a ring with {space} free"
        ));
    }
    let s = out.stats();
    if s.dropped != 100 || s.written != offered as u64 {
        return Err(format!("overrun not counted: {s:?}"));
    }
    pump(offered);
    let s = out.stats();
    if s.written != s.played + s.dropped {
        return Err(format!("written != played + dropped: {s:?}"));
    }
    Ok(())
}

/// `stop` releases the stream, and the output starts again.
fn restart<B: AudioBackend + ?Sized>(
    b: &B,
    id: &EndpointId,
    pump: &mut dyn FnMut(usize),
) -> Result<(), String> {
    let mut out = open(b, id, 1_024)?;
    let mut producer = out.producer().ok_or("no producer")?;
    let channels = producer.channels();
    out.start().map_err(|e| format!("first start: {e}"))?;
    out.stop();
    producer.push(&vec![0.5; 50 * channels]);
    pump(50);
    if out.stats().played != 0 {
        return Err("frames played while stopped".into());
    }
    out.start().map_err(|e| format!("restart: {e}"))?;
    pump(50);
    if out.stats().played != 50 {
        return Err(format!("restart played {} frames", out.stats().played));
    }
    Ok(())
}
