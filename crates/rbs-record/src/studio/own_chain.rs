//! Outputs that own their video encoder chain, and reserved output slots (ADR-0139
//! decisions 8 and 9, spec `docs/specs/recording.md#outputs-that-own-their-chain`).
//!
//! A guest share encodes for one peer: its encoder follows that peer's keyframe
//! requests and congestion estimate, so no other output may share it (ADR-0047
//! decision 6, ADR-0127 decision 4). Between *Share* and the share's start the
//! studio keeps its slots reserved, as bookkeeping only: no node, no encoder.

use std::sync::Arc;

use rbs_codec::{EncoderTuning, RateControl, VideoEncoderConfig};
use rbs_nodes::encode::KeyframeAt;
use rbs_nodes::{BitrateLimiter, EncoderFeedback, VideoEncoderNode};

use crate::recording::{RecordAdapters, VideoSettings};
use crate::select::{self, Chosen};

use super::{
    CanvasId, Chains, MAX_CHAINS, MAX_CHAINS_PER_CANVAS, MAX_OUTPUTS, MAX_OUTPUTS_PER_CANVAS,
    RecordError, Studio,
};

/// An output that owns its video encoder chain ([`OutputSettings::own_chain`]).
///
/// [`OutputSettings::own_chain`]: super::OutputSettings::own_chain
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnChain {
    /// Lowest bitrate the congestion estimate may set, in bits per second.
    pub floor_bps: u64,
    /// Highest bitrate it may set.
    pub max_bps: u64,
}

impl OwnChain {
    /// The bitrate follows the transport's hints between `floor_bps` and `max_bps`.
    pub fn new(floor_bps: u64, max_bps: u64) -> Self {
        Self { floor_bps, max_bps }
    }
}

/// Output and chain slots kept for an output that starts later
/// ([`Studio::reserve_slot`]). Given back by [`Studio::release_slot`], or taken by
/// the start that names it in [`OutputSettings::reserved`].
///
/// [`OutputSettings::reserved`]: super::OutputSettings::reserved
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotReservation(u64);

/// The reservations of a studio.
#[derive(Debug, Default)]
pub(super) struct Reserved {
    next: u64,
    held: Vec<(SlotReservation, CanvasId)>,
}

impl Reserved {
    fn on(&self, canvas: CanvasId) -> usize {
        self.held.iter().filter(|(_, c)| *c == canvas).count()
    }
}

impl Chains {
    /// No free chain slot for a new chain of `canvas`: the canvas runs or reserves
    /// [`MAX_CHAINS_PER_CANVAS`] chains, or all [`MAX_CHAINS`] slots are taken or
    /// reserved.
    pub(super) fn no_chain_slot(&self, canvas: CanvasId) -> bool {
        let on_canvas = self
            .video
            .iter()
            .flatten()
            .filter(|c| c.canvas == canvas)
            .count();
        let running = self.video.iter().flatten().count();
        on_canvas + self.reserved.on(canvas) >= MAX_CHAINS_PER_CANVAS
            || running + self.reserved.held.len() >= MAX_CHAINS
    }
}

impl Studio {
    /// `TooManyOutputs` when no output slot is free for `canvas`, counting
    /// reservations: [`MAX_OUTPUTS`] in all or [`MAX_OUTPUTS_PER_CANVAS`] on it.
    pub(super) fn output_slot(&self, canvas: CanvasId) -> Result<(), RecordError> {
        let reserved = &self.chains.reserved;
        let on_canvas = self.outputs.iter().filter(|o| o.canvas == canvas).count();
        if self.outputs.len() + reserved.held.len() >= MAX_OUTPUTS
            || on_canvas + reserved.on(canvas) >= MAX_OUTPUTS_PER_CANVAS
        {
            return Err(RecordError::TooManyOutputs);
        }
        Ok(())
    }

    /// Reserves one output slot and one video chain slot of `canvas` (the main canvas
    /// for `None`) for an output that starts later with
    /// [`OutputSettings::reserved`](super::OutputSettings::reserved), so other starts
    /// see them as used (ADR-0139 decision 9). Opens nothing. `TooManyOutputs` or
    /// `NoEncoderSlot` as a start would give them now; the error of an unknown
    /// canvas as a start gives it.
    pub fn reserve_slot(
        &mut self,
        canvas: Option<CanvasId>,
    ) -> Result<SlotReservation, RecordError> {
        let canvas = canvas.unwrap_or(self.settings.canvas);
        self.canvas_size(canvas)?;
        self.output_slot(canvas)?;
        if self.chains.no_chain_slot(canvas) {
            return Err(RecordError::NoEncoderSlot);
        }
        let reserved = &mut self.chains.reserved;
        let id = SlotReservation(reserved.next);
        reserved.next += 1;
        reserved.held.push((id, canvas));
        Ok(id)
    }

    /// Gives a reservation back. False when it is unknown (released, or taken by a
    /// start).
    pub fn release_slot(&mut self, id: SlotReservation) -> bool {
        let held = &mut self.chains.reserved.held;
        let before = held.len();
        held.retain(|(r, _)| *r != id);
        held.len() != before
    }

    /// The reservations held.
    pub fn reserved_slots(&self) -> usize {
        self.chains.reserved.held.len()
    }

    /// Takes `id` out of the bookkeeping for a start that uses it; `Err` when unknown
    /// or of another canvas than the output's.
    pub(super) fn take_reservation(
        &mut self,
        id: SlotReservation,
        canvas: CanvasId,
    ) -> Result<(SlotReservation, CanvasId), RecordError> {
        let held = &mut self.chains.reserved.held;
        let at = held
            .iter()
            .position(|&(r, c)| r == id && c == canvas)
            .ok_or(RecordError::NotBound("slot reservation"))?;
        Ok(held.remove(at))
    }

    /// Puts back a reservation whose start failed.
    pub(super) fn restore_reservation(&mut self, entry: (SlotReservation, CanvasId)) {
        self.chains.reserved.held.push(entry);
    }
}

/// The encoder configuration of an output that owns its chain: low-latency tuning
/// (ADR-0139 decision 9). B-frames and the keyframe interval come from its
/// settings.
pub(super) fn tuned(config: VideoEncoderConfig) -> VideoEncoderConfig {
    let mut tuning: EncoderTuning = config.tuning;
    tuning.low_latency = true;
    config.with_tuning(tuning)
}

/// The encoder and the output's feedback of a new chain that `own` owns: keyframe
/// requests always; the bitrate when the adapter lists `bitrate` in
/// `codec.reconfigure` and the rate control has one (otherwise it stays fixed,
/// decision 8).
pub(super) fn feedback(
    own: &OwnChain,
    encoder: VideoEncoderNode,
    keyframes: &Arc<KeyframeAt>,
    video: &VideoSettings,
    adapters: &RecordAdapters,
    chosen: &Chosen,
) -> (VideoEncoderNode, EncoderFeedback) {
    let feedback = EncoderFeedback::new().with_keyframes(Arc::clone(keyframes));
    let capable = select::declares_bitrate(&adapters.video_encoders, &chosen.adapter);
    let start = rate_bps(video.rate_control);
    let limiter = start.and_then(|bps| BitrateLimiter::new(bps, own.floor_bps, own.max_bps));
    match limiter {
        Some(limiter) if capable => {
            let (encoder, target) = encoder.with_bitrate_changes();
            (encoder, feedback.with_bitrate(target, limiter))
        }
        _ => (encoder, feedback),
    }
}

/// The bitrate of a rate control that has one.
fn rate_bps(rate: RateControl) -> Option<u64> {
    match rate {
        RateControl::Cbr { bitrate_bps } | RateControl::CbrPadded { bitrate_bps } => {
            Some(bitrate_bps)
        }
        RateControl::Vbr { target_bps, .. } => Some(target_bps),
        _ => None,
    }
}
