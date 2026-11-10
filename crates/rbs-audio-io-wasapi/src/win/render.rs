//! Audio output (ADR-0097 decision 1): an event-driven shared-mode render stream on
//! its own thread, fed from the port's output ring.
//!
//! `open_output` reads the endpoint's mix format and makes the ring (Standby, P13);
//! `start` runs one render thread (`rbs-wasapi-render`, MMCSS "Pro Audio") that
//! opens the stream, fills the engine buffer with silence, starts it and on every
//! buffer event writes as many frames as the engine has room for: ring frames into the
//! first channels, silence in the rest, and silence counted by the ring where it ran
//! dry. `stop` ends the thread, which hands the ring's consumer back for a restart.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{E_FAIL, GetLastError, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    IAudioClient, IAudioRenderClient,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, WaitForMultipleObjects,
};
use windows::core::w;

use rbs_audio_io::output::{self as out_ring, OutputConsumer, OutputProducer, OutputStatsReader};
use rbs_audio_io::{
    AudioEndpoint, AudioIoError, AudioOutput, EndpointKind, ErrorDomain, OutputRequest, OutputStats,
};
use rbs_core::media::{AudioFormat, MAX_CHANNELS};

use super::{
    Event, START_TIMEOUT, Status, activate_client, device, enumerator, error, hresult, mix_format,
    wave_fields,
};
use crate::wave::{SampleKind, WaveFormat};
use crate::{Owned, ids};

/// The engine buffer asked for, in 100 ns units (20 ms): what the device holds ahead
/// of playback, part of the output latency.
const RENDER_BUFFER: i64 = 200_000;

/// Latency reported before the stream has started and measured it.
pub(super) const TYPICAL_OUTPUT_LATENCY_NS: u64 = 30_000_000;

/// Frames written per pass, at most (the engine buffer is smaller at any common rate).
const MAX_PASS_FRAMES: usize = 8_192;

/// Every active render endpoint whose mix format is 32-bit float.
pub(super) fn endpoints() -> Result<Vec<AudioEndpoint>, AudioIoError> {
    Ok(super::device_endpoints(EndpointKind::Output)?
        .into_iter()
        .filter(|(_, wave)| wave.kind == SampleKind::F32)
        .map(|(endpoint, _)| endpoint)
        .collect())
}

/// Opens `request`'s render endpoint in Standby.
pub(super) fn open(request: &OutputRequest) -> Result<Box<dyn AudioOutput>, AudioIoError> {
    let Some((EndpointKind::Output, native)) = ids::parse(&request.endpoint) else {
        return Err(AudioIoError::EndpointNotFound);
    };
    let device = device(&enumerator()?, EndpointKind::Output, native)?;
    let wave = mix_format(&activate_client(&device)?)?;
    if wave.kind != SampleKind::F32 {
        return Err(AudioIoError::FormatUnsupported);
    }
    let channels = wave.format.channels.channels().clamp(1, MAX_CHANNELS);
    let (producer, consumer) = out_ring::channel(channels, request.clamped_ring_frames());
    Ok(Box::new(WasapiOutput {
        id: native.to_owned(),
        wave,
        reader: consumer.reader(),
        producer: Some(producer),
        consumer: Some(consumer),
        status: Arc::new(Status::default()),
        latency_ns: Arc::new(AtomicU64::new(TYPICAL_OUTPUT_LATENCY_NS)),
        run: None,
        stale: None,
    }))
}

/// A running render thread.
struct Running {
    stop: Arc<Event>,
    thread: JoinHandle<OutputConsumer>,
}

struct WasapiOutput {
    /// The `IMMDevice` id.
    id: String,
    wave: WaveFormat,
    reader: OutputStatsReader,
    producer: Option<OutputProducer>,
    /// Here while stopped; owned by the render thread while it runs.
    consumer: Option<OutputConsumer>,
    status: Arc<Status>,
    latency_ns: Arc<AtomicU64>,
    run: Option<Running>,
    /// A render thread that did not open in time; it ends on its stop event and
    /// hands the consumer back.
    stale: Option<JoinHandle<OutputConsumer>>,
}

/// What the render thread needs.
struct Setup {
    id: String,
    wave: WaveFormat,
    status: Arc<Status>,
    stop: Arc<Event>,
    latency_ns: Arc<AtomicU64>,
}

impl WasapiOutput {
    /// Takes the consumer back from a stale thread once it has ended.
    fn reclaim(&mut self) {
        if self.stale.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(thread) = self.stale.take() {
                if let Ok(consumer) = thread.join() {
                    self.consumer = Some(consumer);
                }
            }
        }
    }
}

impl AudioOutput for WasapiOutput {
    fn format(&self) -> AudioFormat {
        self.wave.format
    }

    fn producer(&mut self) -> Option<OutputProducer> {
        self.producer.take()
    }

    fn start(&mut self) -> Result<(), AudioIoError> {
        if self.run.is_some() {
            return Ok(());
        }
        self.reclaim();
        let Some(consumer) = self.consumer.take() else {
            // The last start timed out and its thread has not ended yet.
            return Err(AudioIoError::DeviceBusy);
        };
        self.status = Arc::new(Status::default());
        let stop = Arc::new(Event::new()?);
        let setup = Setup {
            id: self.id.clone(),
            wave: self.wave,
            status: Arc::clone(&self.status),
            stop: Arc::clone(&stop),
            latency_ns: Arc::clone(&self.latency_ns),
        };
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("rbs-wasapi-render".into())
            .spawn(move || render_thread(setup, consumer, started_tx))
            .map_err(|e| AudioIoError::Backend {
                domain: ErrorDomain::HResult,
                code: i64::from(e.raw_os_error().unwrap_or(0)),
                recoverable: true,
            })?;
        match started_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => {
                self.run = Some(Running { stop, thread });
                Ok(())
            }
            Ok(Err(e)) => {
                // The thread reported and is returning with the consumer.
                if let Ok(consumer) = thread.join() {
                    self.consumer = Some(consumer);
                }
                Err(e)
            }
            Err(_) => {
                // Too slow for the port's budget: the thread sees the stop event or the
                // closed channel and ends on its own, not joined here.
                stop.set();
                self.stale = Some(thread);
                Err(AudioIoError::Backend {
                    domain: ErrorDomain::HResult,
                    // HRESULT_FROM_WIN32(ERROR_TIMEOUT)
                    code: i64::from(0x8007_05B4_u32 as i32),
                    recoverable: true,
                })
            }
        }
    }

    fn stop(&mut self) {
        let Some(run) = self.run.take() else {
            return;
        };
        run.stop.set();
        if let Ok(consumer) = run.thread.join() {
            self.consumer = Some(consumer);
        }
    }

    fn latency_ns(&self) -> u64 {
        self.latency_ns.load(Ordering::Relaxed)
    }

    fn stats(&self) -> OutputStats {
        self.reader.stats()
    }

    fn status(&self) -> Result<(), AudioIoError> {
        match self.status.error() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for WasapiOutput {
    fn drop(&mut self) {
        self.stop();
        if let Some(stale) = self.stale.take() {
            let _ = stale.join();
        }
    }
}

/// A render stream opened on the render thread.
struct Opened {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: Event,
    /// Frames the engine buffer holds.
    buffer_frames: u32,
    period: Duration,
}

fn open_stream(setup: &Setup) -> Result<Opened, AudioIoError> {
    let device = device(&enumerator()?, EndpointKind::Output, &setup.id)?;
    let client = activate_client(&device)?;
    // SAFETY: plain COM calls on a live client; the mix format is valid for the
    // duration of Initialize and freed after it.
    unsafe {
        let mix = client.GetMixFormat().map_err(error)?;
        // The ring and the writes assume the format `open_output` read.
        let same = WaveFormat::parse(wave_fields(mix)).as_ref() == Some(&setup.wave);
        let result = if same {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    RENDER_BUFFER,
                    0,
                    mix,
                    None,
                )
                .map_err(error)
        } else {
            Err(AudioIoError::FormatChanged)
        };
        CoTaskMemFree(Some(mix as _));
        result?;
        let buffer_frames = client.GetBufferSize().map_err(error)?;
        let mut period = 0_i64;
        let period = match client.GetDevicePeriod(Some(&mut period), None) {
            Ok(()) if period > 0 => Duration::from_nanos(period.unsigned_abs() * 100),
            _ => Duration::from_millis(10),
        };
        let rate = u64::from(setup.wave.format.sample_rate.hz().max(1));
        let stream = client
            .GetStreamLatency()
            .map_or(0, |l| l.unsigned_abs() * 100);
        let buffered = u64::from(buffer_frames) * 1_000_000_000 / rate;
        setup.latency_ns.store(stream + buffered, Ordering::Relaxed);
        let render = client.GetService::<IAudioRenderClient>().map_err(error)?;
        let event = Event::new()?;
        client.SetEventHandle(event.0).map_err(error)?;
        Ok(Opened {
            client,
            render,
            event,
            buffer_frames,
            period,
        })
    }
}

/// Fills the engine buffer with silence. Before `Start` only.
fn prefill(opened: &Opened) -> windows::core::Result<()> {
    // SAFETY: plain COM calls on a live render client; no data is written, the
    // buffer is released as silent.
    unsafe {
        let padding = opened.client.GetCurrentPadding()?;
        let room = opened.buffer_frames.saturating_sub(padding);
        if room > 0 {
            opened.render.GetBuffer(room)?;
            opened
                .render
                .ReleaseBuffer(room, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
        }
    }
    Ok(())
}

/// Writes as many frames as the engine has room for. **Real-time**: no allocation,
/// locks, I/O or logging.
fn write(
    opened: &Opened,
    wave: &WaveFormat,
    consumer: &mut OutputConsumer,
    scratch: &mut [f32],
) -> windows::core::Result<()> {
    let ring_channels = consumer.channels();
    let device_channels = wave.source_channels.max(1);
    let max_frames = scratch.len() / ring_channels;
    // SAFETY: plain COM calls on a live render client; the buffer holds `frames`
    // frames of `device_channels` f32 samples (the mix format checked at open) and is
    // written only between GetBuffer and ReleaseBuffer.
    unsafe {
        let padding = opened.client.GetCurrentPadding()?;
        let room = opened.buffer_frames.saturating_sub(padding) as usize;
        let frames = room.min(max_frames);
        if frames == 0 {
            return Ok(());
        }
        let data = opened.render.GetBuffer(frames as u32)?;
        let interleaved = &mut scratch[..frames * ring_channels];
        consumer.pull(interleaved);
        let out = data.cast::<f32>();
        if out.is_aligned() {
            let out = std::slice::from_raw_parts_mut(out, frames * device_channels);
            for (dst, src) in out
                .chunks_exact_mut(device_channels)
                .zip(interleaved.chunks_exact(ring_channels))
            {
                let n = ring_channels.min(device_channels);
                dst[..n].copy_from_slice(&src[..n]);
                dst[n..].fill(0.0);
            }
            opened.render.ReleaseBuffer(frames as u32, 0)?;
        } else {
            // Never seen in practice; silence rather than unaligned writes.
            opened
                .render
                .ReleaseBuffer(frames as u32, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
        }
    }
    Ok(())
}

fn render_thread(
    setup: Setup,
    mut consumer: OutputConsumer,
    started: mpsc::SyncSender<Result<(), AudioIoError>>,
) -> OutputConsumer {
    let _thread = Owned::new();
    // SAFETY: joins the MTA on this thread; balanced by CoUninitialize below.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let mut task = 0_u32;
    // SAFETY: registers this thread with MMCSS; reverted below.
    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task) }.ok();
    run(&setup, &mut consumer, started);
    if let Some(mmcss) = mmcss {
        // SAFETY: the handle from AvSetMmThreadCharacteristicsW on this thread.
        let _ = unsafe { AvRevertMmThreadCharacteristics(mmcss) };
    }
    if com {
        // SAFETY: balances the successful CoInitializeEx above; every COM object of
        // this thread was released by `run`.
        unsafe { CoUninitialize() };
    }
    consumer
}

fn run(
    setup: &Setup,
    consumer: &mut OutputConsumer,
    started: mpsc::SyncSender<Result<(), AudioIoError>>,
) {
    let opened = match open_stream(setup) {
        Ok(opened) => opened,
        Err(e) => {
            let _ = started.send(Err(e));
            return;
        }
    };
    let _stream = Owned::new();
    let channels = consumer.channels();
    let frames = (opened.buffer_frames as usize).min(MAX_PASS_FRAMES);
    // Allocated once, before the stream starts.
    let mut scratch = vec![0.0_f32; frames * channels];
    if let Err(e) = prefill(&opened) {
        let _ = started.send(Err(error(e)));
        return;
    }
    // SAFETY: plain COM call on a live client.
    if let Err(e) = unsafe { opened.client.Start() } {
        let _ = started.send(Err(error(e)));
        return;
    }
    if started.send(Ok(())).is_err() {
        // `start` gave up waiting.
        // SAFETY: plain COM call on a live client.
        let _ = unsafe { opened.client.Stop() };
        return;
    }
    let timeout = u32::try_from((opened.period * 2).as_millis().max(1)).unwrap_or(20);
    let handles = [setup.stop.0, opened.event.0];
    loop {
        // SAFETY: both handles are live for the duration of the wait.
        let woke = unsafe { WaitForMultipleObjects(&handles, false, timeout) };
        match woke.0 {
            0 => break,
            1 => {}
            w if w == WAIT_TIMEOUT.0 => {}
            _ => {
                // SAFETY: no preconditions.
                let mut code = unsafe { GetLastError() }.to_hresult();
                if code.is_ok() {
                    code = E_FAIL;
                }
                setup.status.set(hresult(code));
                break;
            }
        }
        if let Err(e) = write(&opened, &setup.wave, consumer, &mut scratch) {
            setup
                .status
                .set(super::classify(&e, Some(setup.id.as_str())));
            break;
        }
    }
    // SAFETY: plain COM call on a live client.
    let _ = unsafe { opened.client.Stop() };
}
