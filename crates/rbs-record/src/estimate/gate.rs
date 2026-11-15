//! The start gate of separate video files (ADR-0087 decisions 5 and 7, spec
//! `docs/specs/control-api.md` "Protocol 1.54"): which marked sources open a video
//! file, which are skipped, and whether a request with the new start parameters is
//! refused. A pure function of the plan and the limits, decided once before a start
//! makes anything.

use rbs_scene::model::SourceId;

use super::{
    Estimate, EstimatePlan, Level, Limits, Resource, TrackRef, UNKNOWN_SESSIONS_VIDEO_LIMIT, Use,
    estimate, grows_with,
};
use crate::iso::IsoRole;

/// What bounds the number of separate video files that fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitCause {
    /// Encoder capacity: the cap of [`UNKNOWN_SESSIONS_VIDEO_LIMIT`] while an
    /// encoder's session limit or load is unknown, or `encoder_sessions`,
    /// `encoder_throughput` or `cpu_load` over (the gate's `hardware_limit`).
    Encoder,
    /// Another resource that grows with the files (`iso_workers`,
    /// `iso_render_share`, `disk_speed`): the gate's `over_limit`.
    Resource,
    /// No encoder can take a separate video file at all.
    NoEncoder,
}

/// How many separate video files fit and what bounds them, the smaller bound winning:
/// the estimate's "at most N", capped at [`UNKNOWN_SESSIONS_VIDEO_LIMIT`] (or the
/// measured `known_count` when higher) while an encoder's capacity is unknown
/// (ADR-0087 Review, 2026-11-14). `plan` tells whether
/// any encoder can take a file (its video template).
pub fn separate_video_bound(plan: &EstimatePlan, e: &Estimate) -> (usize, LimitCause) {
    if plan.video_template.is_none() {
        return (0, LimitCause::NoEncoder);
    }
    let max = &e.max_video;
    let unknown_capacity = max.unknown.contains(&Resource::EncoderSessions)
        || max.unknown.contains(&Resource::CpuLoad);
    let count = max.count.map(|c| c as usize);
    let cause_of = |limit: Option<Resource>| match limit {
        Some(Resource::EncoderSessions | Resource::EncoderThroughput | Resource::CpuLoad)
        | None => LimitCause::Encoder,
        Some(_) => LimitCause::Resource,
    };
    let cap = max.unknown_capacity_cap();
    match (count, unknown_capacity) {
        (Some(c), true) if c < cap => (c, cause_of(max.limit)),
        (_, true) => (cap, LimitCause::Encoder),
        (Some(c), false) => (c, cause_of(max.limit)),
        (None, false) => (UNKNOWN_SESSIONS_VIDEO_LIMIT, LimitCause::Encoder),
    }
}

/// What the start's hardware-limit policy is (`on_hardware_limit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatePolicy {
    /// Refuse the start and say why.
    Ask,
    /// Skip the files that do not fit.
    Skip,
}

/// The kind of start request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateRequest {
    /// A request with neither new parameter: never refused because of separate video
    /// files; what the gate would stop is skipped.
    Older,
    /// A request with `acknowledge_heavy` or `on_hardware_limit`.
    Asked {
        /// The client accepts a `heavy` plan.
        acknowledge_heavy: bool,
        /// The hardware-limit policy.
        policy: GatePolicy,
    },
}

/// Why the gate refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateRefusal {
    /// More files than the encoder can take, under `ask`.
    HardwareLimit,
    /// Over a limit a CPU encoder would not help with, or a hard limit.
    OverLimit,
    /// `heavy` without `acknowledge_heavy`.
    Heavy,
}

/// The gate's answer.
#[derive(Clone, Debug, PartialEq)]
pub struct GateDecision {
    /// Marked sources whose video files open, in source id order.
    pub keep: Vec<SourceId>,
    /// Marked sources skipped by the gate (`limit`), in source id order.
    pub limit: Vec<SourceId>,
    /// No encoder can take a file: every marked source is skipped (`open_failed`).
    pub no_encoder: Vec<SourceId>,
    /// How many files a request with neither parameter may open, a later marked
    /// source taking the slot of one without a picture (1.52's behaviour).
    pub count: usize,
    /// The estimate of the plan with the kept files.
    pub estimate: Estimate,
    /// A refusal: why, the estimate judged and the files concerned.
    pub refusal: Option<(GateRefusal, Estimate, Vec<TrackRef>)>,
}

/// The plan with only the separate video files of `keep`.
fn with_files(plan: &EstimatePlan, keep: &[SourceId]) -> EstimatePlan {
    let mut p = plan.clone();
    p.video.retain(|e| match e.used_by {
        Use::Iso(t) if t.role == IsoRole::Video => keep.contains(&t.source),
        _ => true,
    });
    p
}

/// The rows of resources that grow with video files at `heavy` or `over`, keyed by
/// resource and scope.
fn heavy_growing(e: &Estimate) -> Vec<(Resource, Option<String>)> {
    e.resources
        .iter()
        .filter(|r| {
            grows_with(r.resource, true)
                && r.resource != Resource::IsoCeiling
                && matches!(r.level, Some(Level::Heavy | Level::Over))
        })
        .map(|r| (r.resource, r.scope.clone()))
        .collect()
}

fn video_track(source: SourceId) -> TrackRef {
    TrackRef {
        source,
        role: IsoRole::Video,
    }
}

/// Decides which of the `marked` video sources (source id order) open a file, for a
/// start of `plan` (every marked file in it) on a computer with `limits` (ADR-0087
/// decisions 5 and 7, control-api "Protocol 1.54"). The gate never looks at disk
/// space ([`Estimate::gate_level`]).
pub fn gate(
    plan: &EstimatePlan,
    limits: &Limits,
    marked: &[SourceId],
    request: GateRequest,
) -> GateDecision {
    let full = estimate(plan, limits);
    let (fit, cause) = separate_video_bound(plan, &full);
    let mut decision = GateDecision {
        keep: Vec::new(),
        limit: Vec::new(),
        no_encoder: Vec::new(),
        count: fit,
        estimate: full.clone(),
        refusal: None,
    };
    if cause == LimitCause::NoEncoder {
        decision.no_encoder = marked.to_vec();
        if let GateRequest::Asked { .. } = request {
            let tracks = marked.iter().copied().map(video_track).collect();
            decision.refusal = Some((GateRefusal::OverLimit, full, tracks));
        }
        return decision;
    }
    let (keep, extra) = marked.split_at(fit.min(marked.len()));
    if let GateRequest::Asked { policy, .. } = request
        && !extra.is_empty()
        && policy == GatePolicy::Ask
    {
        let why = match cause {
            LimitCause::Encoder => GateRefusal::HardwareLimit,
            _ => GateRefusal::OverLimit,
        };
        let tracks = extra.iter().copied().map(video_track).collect();
        decision.refusal = Some((why, full, tracks));
        return decision;
    }
    let mut keep = keep.to_vec();
    let mut limit = extra.to_vec();
    let mut kept = estimate(&with_files(plan, &keep), limits);
    match request {
        GateRequest::Asked {
            acknowledge_heavy, ..
        } => {
            let why = match kept.gate_level {
                Level::Over => Some(GateRefusal::OverLimit),
                Level::Heavy if !acknowledge_heavy => Some(GateRefusal::Heavy),
                _ => None,
            };
            if let Some(why) = why {
                decision.refusal = Some((why, kept.clone(), Vec::new()));
            }
        }
        GateRequest::Older => {
            // Trim while a resource that grows with the files is heavy or over now
            // and was not with no file at all.
            let none = estimate(&with_files(plan, &[]), limits);
            let before = heavy_growing(&none);
            while let Some(last) = keep.last().copied() {
                let raised = heavy_growing(&kept)
                    .into_iter()
                    .any(|r| !before.contains(&r));
                if !raised {
                    break;
                }
                keep.pop();
                limit.insert(0, last);
                kept = estimate(&with_files(plan, &keep), limits);
            }
            decision.count = keep.len();
        }
    }
    limit.sort();
    decision.keep = keep;
    decision.limit = limit;
    decision.estimate = kept;
    decision
}
