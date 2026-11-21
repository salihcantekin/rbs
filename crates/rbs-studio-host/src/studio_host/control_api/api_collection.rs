//! Control API answers, from `CollectionSetTransition` on (ADR-0159 decision 8; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_collection(
        &mut self,
        command: rbs_api::Command,
    ) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::CollectionSetTransition(p) => {
                let changed = host.set_transition(&p).map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::TransitionSet(rbs_api::request::TransitionSet {
                    alpha_unsupported: host.stinger_alpha_unsupported(),
                }))
            }
            C::SourceSetGlide(p) => {
                let (custom, duration_ms, easing) = match p.glide {
                    None => (false, 0, 0),
                    Some(g) => (
                        true,
                        g.duration_ms,
                        match g.easing {
                            api::GlideEasing::Linear => 0,
                            api::GlideEasing::EaseInOut => 1,
                        },
                    ),
                };
                let changed = host
                    .set_source_glide(SourceId::from_raw(p.source), custom, duration_ms, easing)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetRegion(p) => {
                let region = p.region.map(SourceRegion::from_array);
                if host
                    .set_capture_region(SourceId::from_raw(p.source), region)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetRecordSeparately(p) => {
                if host
                    .set_record_separately(SourceId::from_raw(p.source), RecordPatch::from_api(&p))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceGetRegion(p) => {
                let source = host
                    .collection
                    .source(SourceId::from_raw(p.source))
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorKind::NotFound,
                            format!("There is no source {}.", p.source),
                        )
                    })?;
                if SourceKind::of(&source.type_id).is_some_and(|kind| kind.is_audio()) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "A sound source has no picture.",
                    ));
                }
                Ok(Response::SourceRegion(SourceRegionInfo {
                    source: p.source,
                    region: source.region.map(SourceRegion::to_array),
                }))
            }
            C::SourceSetChrome(p) => {
                let chrome = p.chrome.map(|spec| {
                    let fill = match spec.fill {
                        rbs_api::request::ChromeFill::Cover => ChromeFill::Cover,
                        rbs_api::request::ChromeFill::Fit => ChromeFill::Fit,
                    };
                    DisplayChrome::new(spec.top, spec.right, spec.bottom, spec.left).with_fill(fill)
                });
                if host
                    .set_display_chrome(SourceId::from_raw(p.source), chrome)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetActivity(p) => {
                let activity = rbs_scene::activity::SourceActivity::new(
                    p.run.map(activity_run),
                    p.on_return.map(activity_return),
                    p.linger_s,
                );
                if host
                    .set_source_activity(SourceId::from_raw(p.source), activity)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceGetActivity(p) => {
                let source = host
                    .collection
                    .source(SourceId::from_raw(p.source))
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorKind::NotFound,
                            format!("There is no source {}.", p.source),
                        )
                    })?;
                let traits = rbs_sources::activity_traits(&source.type_id);
                let saved = source.activity;
                let effective = traits.effective(Some(&saved));
                Ok(Response::SourceActivity(
                    rbs_api::request::SourceActivityInfo {
                        source: p.source,
                        run: saved.run.map(api_run),
                        on_return: saved.on_return.map(api_return),
                        linger_s: saved.linger_s,
                        effective: rbs_api::request::EffectiveActivity {
                            run: api_run(effective.run),
                            on_return: effective.on_return.map(api_return),
                            linger_s: effective.linger_s,
                        },
                        supported: saved.is_default() || traits.allows(&saved),
                        runs: traits.runs.iter().copied().map(api_run).collect(),
                        on_returns: traits.on_returns.iter().copied().map(api_return).collect(),
                    },
                ))
            }
            C::SourceGetChrome(p) => {
                let source = host
                    .collection
                    .source(SourceId::from_raw(p.source))
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorKind::NotFound,
                            format!("There is no source {}.", p.source),
                        )
                    })?;
                Ok(Response::SourceChrome(SourceChromeInfo {
                    source: p.source,
                    chrome: source.chrome.map(|chrome| ChromeSpec {
                        top: chrome.top,
                        right: chrome.right,
                        bottom: chrome.bottom,
                        left: chrome.left,
                        fill: match chrome.fill {
                            ChromeFill::Cover => rbs_api::request::ChromeFill::Cover,
                            ChromeFill::Fit => rbs_api::request::ChromeFill::Fit,
                            _ => rbs_api::request::ChromeFill::Cover,
                        },
                    }),
                }))
            }
            C::SourceFitChrome(p) => {
                if host
                    .fit_display_chrome(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            // Routed elsewhere by `ApiTarget::run`.
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }
}
