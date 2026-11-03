//! Each wrapper breaks one invariant of the audio I/O port; the suite must catch it.
//! Proves the checks are not vacuous.

use std::sync::Arc;

use rbs_audio_io::conformance::{self, Options, TestPacer};
use rbs_audio_io::{
    AudioBackend, AudioEndpoint, AudioInputStream, AudioIoError, AudioRead, EndpointKind,
    InputRequest, InputStats,
};
use rbs_audio_io_test::TestAudioBackend;
use rbs_core::capability::CapabilitySet;
use rbs_core::media::AudioFormat;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    AllocatesInRead,
    StartsInOpen,
    ShiftsTimestamps,
    NoFirstDiscontinuity,
    HidesDrops,
    DeliversAfterStop,
    IgnoresLoss,
    IgnoresRequestedRate,
    StepsBackwards,
    StampsInTheFuture,
    HidesNewRuns,
}

struct Faulty {
    inner: TestAudioBackend,
    fault: Fault,
}

impl AdapterFactory for Faulty {
    fn id(&self) -> AdapterId {
        AdapterId::new("org.rbs.audio_io.faulty")
    }
    fn display_name(&self) -> &str {
        "faulty"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        self.inner.probe()
    }
}

impl AudioBackend for Faulty {
    fn enumerate(&self, kind: EndpointKind) -> Result<Vec<AudioEndpoint>, AudioIoError> {
        self.inner.enumerate(kind)
    }

    fn open_input(
        &self,
        request: &InputRequest,
    ) -> Result<Box<dyn AudioInputStream>, AudioIoError> {
        let request = if self.fault == Fault::IgnoresRequestedRate {
            request.clone().with_sample_rates(&[])
        } else {
            request.clone()
        };
        let mut inner = self.inner.open_input(&request)?;
        if self.fault == Fault::StartsInOpen {
            inner.start()?;
        }
        Ok(Box::new(FaultyStream {
            inner,
            fault: self.fault,
            reads: 0,
            stopped_tail: None,
        }))
    }
}

struct FaultyStream {
    inner: Box<dyn AudioInputStream>,
    fault: Fault,
    reads: u64,
    stopped_tail: Option<usize>,
}

impl AudioInputStream for FaultyStream {
    fn format(&self) -> AudioFormat {
        self.inner.format()
    }
    fn start(&mut self) -> Result<(), AudioIoError> {
        self.stopped_tail = None;
        self.reads = 0;
        self.inner.start()
    }
    fn stop(&mut self) {
        self.inner.stop();
        if self.fault == Fault::DeliversAfterStop {
            self.stopped_tail = Some(1);
        }
    }
    fn read(&mut self, out: &mut [&mut [f32]]) -> Result<AudioRead, AudioIoError> {
        if let Some(n) = self.stopped_tail.take() {
            let _ = self.inner.read(out)?;
            return Ok(AudioRead::new(
                n,
                rbs_core::time::Timestamp::from_nanos(u64::MAX / 2),
                false,
            ));
        }
        if self.fault == Fault::AllocatesInRead {
            std::hint::black_box(Box::new(1_u8));
        }
        let mut read = match self.inner.read(out) {
            Err(AudioIoError::EndpointLost) if self.fault == Fault::IgnoresLoss => {
                return Ok(AudioRead::EMPTY);
            }
            other => other?,
        };
        if read.frames > 0 {
            self.reads += 1;
            match self.fault {
                Fault::ShiftsTimestamps => {
                    read = AudioRead::new(
                        read.frames,
                        read.timestamp + std::time::Duration::from_millis(5 * (self.reads % 2)),
                        read.discontinuity,
                    );
                }
                Fault::NoFirstDiscontinuity => {
                    read = AudioRead::new(read.frames, read.timestamp, false);
                }
                Fault::StepsBackwards if self.reads == 3 => {
                    read = AudioRead::new(
                        read.frames,
                        read.timestamp - std::time::Duration::from_millis(100),
                        read.discontinuity,
                    );
                }
                Fault::StampsInTheFuture => {
                    read = AudioRead::new(
                        read.frames,
                        read.timestamp + std::time::Duration::from_secs(1),
                        read.discontinuity,
                    );
                }
                Fault::HidesNewRuns if self.reads > 1 => {
                    read = AudioRead::new(read.frames, read.timestamp, false);
                }
                _ => {}
            }
        }
        Ok(read)
    }
    fn available(&self) -> usize {
        self.inner.available()
    }
    fn stats(&self) -> InputStats {
        let mut stats = self.inner.stats();
        if self.fault == Fault::HidesDrops {
            stats.captured -= stats.dropped;
            stats.dropped = 0;
        }
        stats
    }
}

fn failures(fault: Fault) -> Vec<String> {
    let backend = Arc::new(Faulty {
        inner: TestAudioBackend::new(),
        fault,
    });
    let options = Options {
        measure_allocations: Some(measure),
        hooks: Some(&backend.inner),
        strict: true,
        ..Options::default()
    };
    let report = conformance::run(backend.as_ref(), &mut TestPacer::new(), &options);
    report.failures().map(|c| c.name.clone()).collect()
}

fn assert_caught(fault: Fault, case: &str) {
    let failed = failures(fault);
    assert!(
        failed.iter().any(|name| name == case),
        "{fault:?} was not caught by {case}: failures {failed:?}"
    );
}

#[test]
fn allocation_in_read_is_caught() {
    assert_caught(Fault::AllocatesInRead, "no allocation");
}

#[test]
fn starting_in_open_is_caught() {
    assert_caught(Fault::StartsInOpen, "standby");
}

#[test]
fn broken_timestamps_are_caught() {
    assert_caught(Fault::ShiftsTimestamps, "timestamps");
}

#[test]
fn missing_first_discontinuity_is_caught() {
    assert_caught(Fault::NoFirstDiscontinuity, "timestamps");
    assert_caught(Fault::NoFirstDiscontinuity, "restart");
}

#[test]
fn hidden_drops_are_caught() {
    assert_caught(Fault::HidesDrops, "overflow");
}

#[test]
fn delivery_after_stop_is_caught() {
    assert_caught(Fault::DeliversAfterStop, "stop");
}

#[test]
fn ignored_endpoint_loss_is_caught() {
    assert_caught(Fault::IgnoresLoss, "endpoint lost");
}

#[test]
fn ignored_rate_request_is_caught() {
    assert_caught(Fault::IgnoresRequestedRate, "format");
}

#[test]
fn backward_timestamps_are_caught() {
    assert_caught(Fault::StepsBackwards, "timestamps");
}

#[test]
fn timestamps_ahead_of_the_clock_are_caught() {
    assert_caught(Fault::StampsInTheFuture, "timestamps");
}

#[test]
fn a_new_run_presented_as_continuous_is_caught() {
    assert_caught(Fault::HidesNewRuns, "overflow");
}
