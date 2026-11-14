//! The plan of the separate video files a start would open, for the estimate
//! (ADR-0087 decision 1, spec `docs/specs/output-estimate.md#the-studios-plan`).

use rbs_codec::RateControl;
use rbs_codec::caps as codec_caps;
use rbs_core::capability::CapabilityValue;
use rbs_core::media::Size;
use rbs_core::media::VideoFormat;
use rbs_core::registry::HARDWARE;
use rbs_scene::SceneCollection;

use crate::estimate::{
    AudioEncode, DEFAULT_PLANNED_DURATION, DeclaredAdapter, EstimatePlan, RateClass, TrackRef, Use,
    VideoEncode,
};
use crate::iso::IsoRole;
use crate::recording::{RecordAdapters, VideoSettings};
use crate::select;

use super::IsoVideoStart;
use super::iso_video::SHARED_ENGINE_PARTITIONS;

/// Bytes per second of a rate control; constant quality counts as `fallback_bps`.
fn bytes_per_second(rate: &RateControl, fallback_bps: u64) -> u64 {
    let bps = match *rate {
        RateControl::Cbr { bitrate_bps } | RateControl::CbrPadded { bitrate_bps } => bitrate_bps,
        RateControl::Vbr { target_bps, .. } => target_bps,
        _ => fallback_bps,
    };
    bps / 8
}

fn rate_class(rate: &RateControl) -> RateClass {
    match rate {
        RateControl::Cbr { .. } | RateControl::CbrPadded { .. } => RateClass::Cbr,
        RateControl::Vbr { .. } => RateClass::Vbr,
        _ => RateClass::Cq,
    }
}

/// A constant-quality encode is planned at this many bits per pixel per frame (a
/// generous 1080p60 recording is about 0.1).
const CQ_BITS_PER_PIXEL: f64 = 0.1;

fn cq_guess(width: u32, height: u32, fps: f64) -> u64 {
    (f64::from(width) * f64::from(height) * fps * CQ_BITS_PER_PIXEL) as u64
}

fn declared(adapters: &RecordAdapters, id: &rbs_core::registry::AdapterId) -> DeclaredAdapter {
    let caps = adapters.video_encoders.capabilities(id);
    let int = |key| match caps.and_then(|c| c.get(key)) {
        Some(CapabilityValue::Int(n)) => Some(*n),
        _ => None,
    };
    DeclaredAdapter {
        id: id.as_str().to_owned(),
        hardware: matches!(
            caps.and_then(|c| c.get(&HARDWARE)),
            Some(CapabilityValue::Flag(true))
        ),
        sessions_max: int(&codec_caps::SESSIONS_MAX).and_then(|n| u32::try_from(n).ok()),
        // `codec.video.pixel_rate.max` is not a port key yet (ADR-0087 consequences).
        pixel_rate_max: None,
    }
}

impl super::Studio {
    /// What a start of the separate video files of `start` would run next to the
    /// outputs that already run, and what their adapters declare (ADR-0087 decision
    /// 1). Reads only the studio's state and the registries' probe results: nothing
    /// opens. The files go to the encoder the selection would most likely use (the
    /// first candidate a running encoder uses, else the first candidate); marked sources
    /// without a picture yet are planned at the canvas size.
    pub fn separate_video_plan(
        &self,
        adapters: &RecordAdapters,
        collection: &SceneCollection,
        start: &IsoVideoStart,
    ) -> (EstimatePlan, Vec<DeclaredAdapter>) {
        let fps = 1.0 / self.fps.frame_duration().as_secs_f64();
        let mut ids: Vec<rbs_core::registry::AdapterId> = Vec::new();
        let note = |ids: &mut Vec<rbs_core::registry::AdapterId>,
                    id: &rbs_core::registry::AdapterId| {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        };

        let mut video = Vec::new();
        for chain in self.chains.video.iter().flatten() {
            note(&mut ids, &chain.chosen.adapter);
            video.push(self.encode_of(
                adapters,
                Use::Recording,
                chain.chosen.adapter.as_str(),
                &chain.settings,
                chain.size,
                fps,
            ));
        }
        let audio: Vec<AudioEncode> = self
            .chains
            .audio
            .iter()
            .flatten()
            .map(|chain| AudioEncode {
                used_by: Use::Recording,
                bytes_per_second: chain.settings.bitrate_bps / 8,
            })
            .collect();

        let (width, height) = self.size;
        let config = super::video_encoder_config(
            &start.video,
            VideoFormat::new(
                Size::new(width, height),
                start.video.format.pixel_format(),
                start.video.color,
            ),
            self.fps,
        );
        let iso_adapter = select::likely_video_encoder(&adapters.video_encoders, &config, &ids);
        if let Some(id) = &iso_adapter {
            note(&mut ids, id);
        }

        let marked = crate::iso::plan(collection)
            .map(|p| p.tracks)
            .unwrap_or_default();
        let mut images = Vec::new();
        let mut i = 0;
        for t in &marked {
            let track = TrackRef {
                source: t.source,
                role: t.role,
            };
            match t.role {
                IsoRole::Image => images.push(track),
                IsoRole::Video => {
                    if let Some(id) = &iso_adapter {
                        let size = self.picture_size(t.source).unwrap_or(self.size);
                        let mut e = self.encode_of(
                            adapters,
                            Use::Iso(track),
                            id.as_str(),
                            &start.video,
                            size,
                            fps,
                        );
                        e.worker = Some(i % SHARED_ENGINE_PARTITIONS);
                        e.export_buffers = rbs_gpu::download::DOWNLOAD_BUFFERS as u32;
                        video.push(e);
                        i += 1;
                    }
                }
                IsoRole::Audio => {}
            }
        }
        let video_template = iso_adapter.as_ref().map(|id| {
            let mut e = self.encode_of(
                adapters,
                Use::Iso(TrackRef {
                    source: rbs_scene::model::SourceId::from_raw(0),
                    role: IsoRole::Video,
                }),
                id.as_str(),
                &start.video,
                self.size,
                fps,
            );
            e.export_buffers = rbs_gpu::download::DOWNLOAD_BUFFERS as u32;
            e
        });

        let plan = EstimatePlan {
            video,
            audio,
            images,
            active_sources: u32::try_from(marked.len()).unwrap_or(u32::MAX),
            inactive: Vec::new(),
            video_template,
            audio_template: None,
            iso_workers: SHARED_ENGINE_PARTITIONS,
            tick: self.fps.frame_duration(),
            give_way_share: start.give_way_share,
            planned_duration: DEFAULT_PLANNED_DURATION,
        };
        let declared = ids.iter().map(|id| declared(adapters, id)).collect();
        (plan, declared)
    }

    fn encode_of(
        &self,
        adapters: &RecordAdapters,
        used_by: Use,
        adapter: &str,
        settings: &VideoSettings,
        size: (u32, u32),
        fps: f64,
    ) -> VideoEncode {
        let hardware = adapters
            .video_encoders
            .ids()
            .find(|id| id.as_str() == adapter)
            .is_some_and(|id| declared(adapters, id).hardware);
        VideoEncode {
            used_by,
            adapter: adapter.to_owned(),
            hardware,
            codec: settings.codec.as_str().to_owned(),
            width: size.0,
            height: size.1,
            fps,
            rate: rate_class(&settings.rate_control),
            bytes_per_second: bytes_per_second(
                &settings.rate_control,
                cq_guess(size.0, size.1, fps),
            ),
            falls_to_cpu: false,
            worker: None,
            export_buffers: 0,
        }
    }
}
