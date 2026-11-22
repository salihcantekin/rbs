//! The control API's types from the host's and the scene model's, and back (ADR-0159 decision 8).

use super::*;
use rbs_scene::activity::OnReturn;

/// Frames per second, rounded to the nearest whole number.
pub(super) fn rounded_rate(fps: FrameRate) -> u32 {
    let rate = fps.per_second();
    (u64::from(rate.num()) + u64::from(rate.den()) / 2)
        .checked_div(u64::from(rate.den()))
        .map_or(0, |r| u32::try_from(r).unwrap_or(u32::MAX))
}

/// An estimate enum as the API's, through its wire name: a name the API types do
/// not know reads as their `Other`.
pub(super) fn api_enum<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()
}

pub(super) fn api_resource(r: rbs_record::estimate::Resource) -> api::EstimateResource {
    api_enum(r.as_str()).unwrap_or(api::EstimateResource::Other)
}

pub(super) fn api_level(l: rbs_record::estimate::Level) -> api::EstimateLevel {
    api_enum(l.as_str()).unwrap_or(api::EstimateLevel::Other)
}

/// A number the API may send: finite, else `None` (ADR-0087: never NaN or infinity).
pub(super) fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

/// `record.estimate`'s answer from the estimate (protocol 1.53). `at_most_now` is the
/// most separate video files a start opens now, when the plan has a picture template.
pub(super) fn api_estimate(
    e: &rbs_record::estimate::Estimate,
    at_most_now: Option<usize>,
) -> api::EstimateAnswer {
    use rbs_record::estimate as est;
    let track = |t: &est::TrackRef| api::EstimateTrack {
        source: t.source.get(),
        role: api_track_role(t.role),
    };
    let max = |m: &est::MaxTracks, now: Option<usize>| api::MaxTracks {
        count: m.count,
        limit: m.limit.map(api_resource),
        partial: m.partial,
        unknown: m.unknown.iter().copied().map(api_resource).collect(),
        at_most_now: now.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
        known_count: m.known_count,
    };
    api::EstimateAnswer {
        level: api_level(e.level),
        gate_level: api_level(e.gate_level),
        unknown: e.unknown.iter().copied().map(api_resource).collect(),
        resources: e
            .resources
            .iter()
            .map(|r| {
                let need = finite(r.need).unwrap_or(0.0);
                let limit = r.limit.and_then(finite);
                // A limit that is not a finite number is reported as unknown.
                let level = match (r.limit, limit) {
                    (Some(_), None) => Some(api::EstimateLevel::Unknown),
                    _ => r.level.map(api_level),
                };
                api::EstimateRow {
                    resource: api_resource(r.resource),
                    scope: r.scope.clone(),
                    need,
                    limit,
                    unit: api_enum(r.unit.as_str()).unwrap_or(api::EstimateUnit::Other),
                    level,
                    measured: r.measured,
                    calibrate: r.calibrate,
                }
            })
            .collect(),
        reasons: e
            .reasons
            .iter()
            .map(|r| api::EstimateReason {
                code: api_enum(r.code.as_str()).unwrap_or(api::EstimateCode::Other),
                resource: r.resource.map(api_resource),
                track: r.track.as_ref().map(track),
            })
            .collect(),
        max_tracks: api::MaxTracksByKind {
            video: max(&e.max_video, at_most_now),
            audio: max(&e.max_audio, None),
        },
        tracks: e
            .tracks
            .iter()
            .map(|t| api::TrackCost {
                source: t.track.source.get(),
                role: api_track_role(t.track.role),
                cost: t
                    .cost
                    .iter()
                    .filter_map(|(r, v)| Some((r.as_str().to_owned(), finite(*v)?)))
                    .collect(),
            })
            .collect(),
    }
}

/// `failed` with `data.reason` (protocol 1.100, ADR-0164 decision 3): the kind
/// stays, so an older client reads it as before.
pub(super) fn failed_because(reason: &str, message: impl Into<String>) -> ApiError {
    let mut data = serde_json::Map::new();
    data.insert("reason".into(), reason.into());
    ApiError::with_data(ErrorKind::Failed, message, data)
}

pub(super) fn failed(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::Failed, message)
}

/// The control API's name of a kind (protocol 1.34 names the picture kinds too).
pub(super) fn api_kind(k: SourceKind) -> Option<api::SourceKind> {
    Some(match k {
        SourceKind::Screen => api::SourceKind::Screen,
        SourceKind::Window => api::SourceKind::Window,
        SourceKind::Camera => api::SourceKind::Camera,
        SourceKind::AudioInput => api::SourceKind::AudioInput,
        SourceKind::SystemAudio => api::SourceKind::SystemAudio,
        SourceKind::ApplicationAudio => api::SourceKind::ApplicationAudio,
        SourceKind::Image => api::SourceKind::Image,
        SourceKind::Color => api::SourceKind::Color,
        SourceKind::Media => api::SourceKind::Media,
        SourceKind::Browser => api::SourceKind::Browser,
        SourceKind::Text => api::SourceKind::Text,
        SourceKind::Slideshow => api::SourceKind::Slideshow,
    })
}

pub(super) fn app_kind(k: api::SourceKind) -> SourceKind {
    match k {
        api::SourceKind::Screen => SourceKind::Screen,
        api::SourceKind::Window => SourceKind::Window,
        api::SourceKind::Camera => SourceKind::Camera,
        api::SourceKind::AudioInput => SourceKind::AudioInput,
        api::SourceKind::SystemAudio => SourceKind::SystemAudio,
        api::SourceKind::ApplicationAudio => SourceKind::ApplicationAudio,
        api::SourceKind::Image => SourceKind::Image,
        api::SourceKind::Color => SourceKind::Color,
        api::SourceKind::Media => SourceKind::Media,
        api::SourceKind::Browser => SourceKind::Browser,
        api::SourceKind::Text => SourceKind::Text,
        api::SourceKind::Slideshow => SourceKind::Slideshow,
    }
}

/// The control API's name of a look group. A group added after protocol 1.34 is `other`.
pub(super) fn api_look_group(group: rbs_scene::look::LookGroup) -> rbs_api::request::LookGroup {
    match group {
        rbs_scene::look::LookGroup::Color => rbs_api::request::LookGroup::Color,
        rbs_scene::look::LookGroup::Effect => rbs_api::request::LookGroup::Effect,
        rbs_scene::look::LookGroup::Shape => rbs_api::request::LookGroup::Shape,
        _ => rbs_api::request::LookGroup::Other,
    }
}

/// The control API's form of a style (ADR-0053).
pub fn api_style(s: &ItemStyle) -> api::ItemStyle {
    let (shape, radius) = match s.shape {
        Shape::Rectangle { radius } => (api::ShapeKind::Rectangle, radius),
        Shape::Circle => (api::ShapeKind::Circle, 0.0),
        Shape::Ellipse => (api::ShapeKind::Ellipse, 0.0),
        Shape::Triangle => (api::ShapeKind::Triangle, 0.0),
        Shape::Arrow => (api::ShapeKind::Arrow, 0.0),
        _ => (api::ShapeKind::Rectangle, 0.0),
    };
    api::ItemStyle {
        shape,
        radius,
        border: s.border.map(|b| api::StyleBorder {
            width: b.width,
            color: api::format_color(b.color),
        }),
        shadow: s.shadow.map(|sh| api::StyleShadow {
            offset: sh.offset,
            blur: sh.blur,
            color: api::format_color(sh.color),
        }),
    }
}

/// The API form of a scroll (ADR-0150).
pub fn api_scroll(s: &Scroll) -> api::ItemScroll {
    api::ItemScroll {
        direction: match s.direction {
            ScrollDirection::Right => api::ScrollDirection::Right,
            ScrollDirection::Up => api::ScrollDirection::Up,
            ScrollDirection::Down => api::ScrollDirection::Down,
            _ => api::ScrollDirection::Left,
        },
        speed: s.speed,
        gap: s.gap,
        repeat: match s.repeat {
            ScrollRepeat::Once => api::ScrollRepeat::Once,
            _ => api::ScrollRepeat::Loop,
        },
        start: match s.start {
            ScrollStart::Outside => api::ScrollStart::Outside,
            _ => api::ScrollStart::Inside,
        },
        fade: s.fade,
        ramp_ms: s.ramp_ms,
        on_return: match s.on_return {
            OnReturn::Continue => api::ScrollReturn::Continue,
            _ => api::ScrollReturn::Restart,
        },
    }
}

/// The scene's form of an API scroll; the error says what is out of range.
pub(super) fn scene_scroll(s: &api::ItemScroll) -> Result<Scroll, Refusal> {
    let direction = match s.direction {
        api::ScrollDirection::Left => ScrollDirection::Left,
        api::ScrollDirection::Right => ScrollDirection::Right,
        api::ScrollDirection::Up => ScrollDirection::Up,
        api::ScrollDirection::Down => ScrollDirection::Down,
    };
    let scroll = Scroll::new(direction, s.speed)
        .with_gap(s.gap)
        .with_repeat(match s.repeat {
            api::ScrollRepeat::Loop => ScrollRepeat::Loop,
            api::ScrollRepeat::Once => ScrollRepeat::Once,
        })
        .with_start(match s.start {
            api::ScrollStart::Inside => ScrollStart::Inside,
            api::ScrollStart::Outside => ScrollStart::Outside,
        })
        .with_fade(s.fade)
        .with_ramp_ms(s.ramp_ms)
        .with_on_return(match s.on_return {
            api::ScrollReturn::Restart => OnReturn::Restart,
            api::ScrollReturn::Continue => OnReturn::Continue,
        });
    if scroll.is_valid() {
        Ok(scroll)
    } else {
        Err(Refusal::Invalid(SCROLL_ERROR.into()))
    }
}

/// Why a scroll is refused.
pub(super) const SCROLL_ERROR: &str =
    "Scroll speed must be 1 to 4000, gap 0 to 16384, edge fade 0 to 2048 and ramp 0 to 5000 ms.";

/// The scene's form of an API style; the error says what is wrong.
pub(super) fn scene_style(s: &api::ItemStyle) -> Result<ItemStyle, Refusal> {
    let shape = match s.shape {
        api::ShapeKind::Rectangle => Shape::Rectangle { radius: s.radius },
        _ if s.radius != 0.0 => {
            return Err(Refusal::Invalid(
                "radius is only for shape \"rectangle\"".to_owned(),
            ));
        }
        api::ShapeKind::Circle => Shape::Circle,
        api::ShapeKind::Ellipse => Shape::Ellipse,
        api::ShapeKind::Triangle => Shape::Triangle,
        api::ShapeKind::Arrow => Shape::Arrow,
    };
    let color = |c: &str| {
        api::parse_color(c)
            .ok_or_else(|| Refusal::Invalid(format!("colour {c:?} is not #rrggbb or #rrggbbaa")))
    };
    let border = match &s.border {
        Some(b) => Some(Border::new(b.width, color(&b.color)?)),
        None => None,
    };
    let shadow = match &s.shadow {
        Some(sh) => Some(Shadow::new(sh.offset, sh.blur, color(&sh.color)?)),
        None => None,
    };
    let style = ItemStyle::new(shape)
        .with_border(border)
        .with_shadow(shadow);
    if style.is_valid() {
        Ok(style)
    } else {
        Err(Refusal::Invalid(STYLE_ERROR.into()))
    }
}

pub(super) fn api_track_role(role: rbs_record::IsoRole) -> api::TrackRole {
    match role {
        rbs_record::IsoRole::Audio => api::TrackRole::Audio,
        rbs_record::IsoRole::Image => api::TrackRole::Image,
        _ => api::TrackRole::Video,
    }
}

pub(super) fn api_picture_form(form: VideoAs) -> api::PictureForm {
    match form {
        VideoAs::Video => api::PictureForm::Video,
        VideoAs::Image => api::PictureForm::Image,
        _ => api::PictureForm::Auto,
    }
}

/// Nanoseconds as milliseconds, to the microsecond (control API 1.77).
pub(super) fn ns_to_ms(ns: i64) -> f64 {
    (ns as f64 / 1_000.0).round() / 1_000.0
}

/// `info` with the counters of its source's audio input, when one runs (`source.list`,
/// protocol 1.77, ADR-0130 decision 9), and its last warm-up (protocol 1.98).
pub(super) fn with_input_counters(studio: &Studio, mut info: api::SourceInfo) -> api::SourceInfo {
    if let Some(c) = studio.input_counters(SourceId::from_raw(info.id)) {
        info.input_late_frames = Some(c.late_frames);
        info.input_resyncs = Some(c.resyncs);
    }
    // Live state like the counters (ADR-0152 decision 10): only `source.list` asks.
    info.warm_up_ms = studio
        .warm_up(SourceId::from_raw(info.id))
        .map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX));
    info
}

pub(super) fn api_source(row: &SourceRow) -> api::SourceInfo {
    api::SourceInfo {
        item: Some(row.item.get()),
        layout: row.layout.map(|l| api::ItemLayout {
            position: l.position,
            size: l.size,
            rotation: l.rotation,
            crop: [l.crop.left, l.crop.top, l.crop.right, l.crop.bottom],
            opacity: l.opacity,
            style: api_style(&l.style),
            scroll: l.scroll.map(|s| api_scroll(&s)),
        }),
        picture_size: row.picture.map(|(w, h)| [w, h]),
        visible: row.visible,
        locked: row.locked,
        volume: row.audio.volume,
        muted: row.audio.muted,
        mixer_hidden: row.audio.mixer_hidden,
        monitor: monitor::api_mode(row.audio.monitor),
        companion_of: row.companion_of.map(|c| c.get()),
        // 1.46's meaning: the main feed (a picture source's picture, anything
        // else's sound), so an older client's switch shows and clears that feed.
        record_separately: if row
            .kind
            .is_some_and(|k| !k.is_audio() && k != SourceKind::Media)
        {
            row.record_separately.video
        } else {
            row.record_separately.audio
        },
        record_tracks: (!row.record_separately.is_none()).then(|| api::RecordTracks {
            video: row.record_separately.video,
            audio: row.record_separately.audio,
            video_as: api_picture_form(row.record_separately.video_as),
        }),
        filters: row.filters.iter().map(filters::api_row).collect(),
        sync_offset_ms: row.audio.sync_offset_ms,
        glide: row.glide.map(api_glide_source),
        heard: row.heard.map(|h| api::Heard {
            volume: h.volume,
            muted: h.muted,
        }),
        filter_latency_ms: row.latency.map_or(0.0, |l| ns_to_ms(l.filter_latency_ns)),
        effective_offset_ms: row.latency.map_or(0.0, |l| ns_to_ms(l.heard_offset_ns)),
        uncompensated_ms: row.latency.map_or(0.0, |l| ns_to_ms(l.uncompensated_ns)),
        // Growing counters: only `source.list` reads them (`with_input_counters`), so
        // a row's change event is not sent on every block.
        input_late_frames: None,
        input_resyncs: None,
        warm_up_ms: None,
        id: row.id.get(),
        name: row.name.clone(),
        kind: row.kind.and_then(api_kind),
        status: match &row.status {
            SourceStatus::Running => api::SourceStatus::Running,
            SourceStatus::Idle => api::SourceStatus::Idle,
            SourceStatus::Failed(why) => api::SourceStatus::Failed {
                reason: why.clone(),
            },
            // The typed state of protocol 1.20 (ADR-0077, decision 7).
            SourceStatus::Unavailable { fault, detail } => api::SourceStatus::Unavailable {
                fault: api_fault(*fault),
                detail: detail.clone(),
            },
        },
    }
}

pub(super) fn api_glide(g: rbs_scene::CollectionGlide) -> api::Glide {
    api::Glide {
        duration_ms: g.duration_ms,
        easing: api_easing(g.easing),
    }
}

pub(super) fn api_glide_source(g: rbs_scene::SourceGlide) -> api::Glide {
    api::Glide {
        duration_ms: g.duration_ms,
        easing: api_easing(g.easing),
    }
}

/// A one-off transition of `canvas.transition` or `scene.switch` (ADR-0128 decision
/// 13), refused when its duration is out of range.
pub(super) fn transition_once(
    p: rbs_api::request::TransitionOnce,
) -> Result<TransitionOnce, Refusal> {
    if let Some(ms) = p.duration_ms
        && ms > rbs_scene::MAX_GLIDE_MS
    {
        return Err(Refusal::Invalid(format!(
            "duration_ms must be at most {}.",
            rbs_scene::MAX_GLIDE_MS
        )));
    }
    Ok(TransitionOnce::new(
        p.kind.map(scene_transition_kind),
        p.duration_ms,
    ))
}

pub(super) fn scene_transition_kind(k: api::TransitionKind) -> rbs_scene::TransitionKind {
    match k {
        api::TransitionKind::Cut => rbs_scene::TransitionKind::Cut,
        api::TransitionKind::Fade => rbs_scene::TransitionKind::Fade,
        api::TransitionKind::Move => rbs_scene::TransitionKind::Move,
        api::TransitionKind::Swipe => rbs_scene::TransitionKind::Swipe,
        api::TransitionKind::Slide => rbs_scene::TransitionKind::Slide,
        api::TransitionKind::Stinger => rbs_scene::TransitionKind::Stinger,
    }
}

pub(super) fn api_transition_kind(k: rbs_scene::TransitionKind) -> api::TransitionKind {
    match k {
        rbs_scene::TransitionKind::Cut => api::TransitionKind::Cut,
        rbs_scene::TransitionKind::Fade => api::TransitionKind::Fade,
        rbs_scene::TransitionKind::Swipe => api::TransitionKind::Swipe,
        rbs_scene::TransitionKind::Slide => api::TransitionKind::Slide,
        rbs_scene::TransitionKind::Stinger => api::TransitionKind::Stinger,
        _ => api::TransitionKind::Move,
    }
}

pub(super) fn scene_easing(e: api::GlideEasing) -> rbs_scene::GlideEasing {
    match e {
        api::GlideEasing::Linear => rbs_scene::GlideEasing::Linear,
        api::GlideEasing::EaseInOut => rbs_scene::GlideEasing::EaseInOut,
    }
}

pub(super) fn scene_swipe(s: api::SwipeTransition) -> rbs_scene::SwipeSettings {
    rbs_scene::SwipeSettings {
        duration_ms: s.duration_ms,
        easing: scene_easing(s.easing),
        direction: match s.direction {
            api::SwipeDirection::Left => rbs_scene::SwipeDirection::Left,
            api::SwipeDirection::Right => rbs_scene::SwipeDirection::Right,
            api::SwipeDirection::Up => rbs_scene::SwipeDirection::Up,
            api::SwipeDirection::Down => rbs_scene::SwipeDirection::Down,
        },
    }
}

pub(super) fn api_swipe(s: rbs_scene::SwipeSettings) -> api::SwipeTransition {
    api::SwipeTransition {
        duration_ms: s.duration_ms,
        easing: api_easing(s.easing),
        direction: match s.direction {
            rbs_scene::SwipeDirection::Right => api::SwipeDirection::Right,
            rbs_scene::SwipeDirection::Up => api::SwipeDirection::Up,
            rbs_scene::SwipeDirection::Down => api::SwipeDirection::Down,
            _ => api::SwipeDirection::Left,
        },
    }
}

/// The collection's transition as `scene.list` shows it (protocol 1.81; the stinger
/// since 1.89).
pub(super) fn api_transition(
    t: rbs_scene::CollectionTransition,
    cut_found_ms: Option<u32>,
) -> api::Transition {
    api::Transition {
        kind: api_transition_kind(t.kind),
        direct_kind: api_transition_kind(t.direct),
        fade: api::FadeTransition {
            duration_ms: t.fade.duration_ms,
            easing: api_easing(t.fade.easing),
            via: t
                .fade
                .via
                .map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}")),
        },
        swipe: api_swipe(t.swipe),
        slide: api_swipe(t.slide),
        stinger: api::StingerTransition {
            source: t.stinger.source.map(SourceId::get),
            cut_ms: t.stinger.cut_ms,
            cut_found_ms: t.stinger.source.and(cut_found_ms),
        },
    }
}

/// `#rrggbb` as bytes.
pub(super) fn parse_rgb(s: &str) -> Option<[u8; 3]> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

pub(super) fn api_easing(e: rbs_scene::GlideEasing) -> api::GlideEasing {
    match e {
        rbs_scene::GlideEasing::Linear => api::GlideEasing::Linear,
        // New variants map to today's default so an older app still talks.
        _ => api::GlideEasing::EaseInOut,
    }
}

/// The control API's name for a source fault.
pub(super) fn api_fault(fault: rbs_core::fault::SourceFault) -> api::SourceFault {
    use rbs_core::fault::SourceFault as F;
    match fault {
        F::TargetGone => api::SourceFault::TargetGone,
        F::PermissionDenied => api::SourceFault::PermissionDenied,
        F::Busy => api::SourceFault::Busy,
        _ => api::SourceFault::Other,
    }
}

/// `scene.list` and `scene.changed` (protocol 1.6).
pub(super) fn api_scenes(list: &SceneList) -> api::SceneList {
    api::SceneList {
        scenes: list
            .scenes
            .iter()
            .map(|s| api::SceneInfo {
                id: s.id.get(),
                name: s.name.clone(),
                own_audio: s.own_audio,
                transition_in: s.transition_in.map(|t| api::SceneTransition {
                    kind: t.kind.map(api_transition_kind),
                    duration_ms: t.duration_ms,
                }),
            })
            .collect(),
        program: list.program.get(),
        preview: list.preview.map(SceneId::get),
        master: api::Master {
            volume: list.master.volume,
            muted: list.master.muted,
        },
        glide: api_glide(list.glide),
        transition: api_transition(list.transition, list.stinger_cut_found_ms),
        sources: list
            .sources
            .iter()
            .map(|s| api::SourceEntry {
                id: s.id.get(),
                name: s.name.clone(),
                kind: s.kind.and_then(api_kind),
                global: s.global,
                glide: s.glide.map(api_glide_source),
            })
            .collect(),
    }
}
