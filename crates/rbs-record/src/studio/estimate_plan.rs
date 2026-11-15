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
use crate::recording::{AudioSettings, RecordAdapters, VideoSettings};

use crate::select;

use super::{OutputId, OutputSettings};

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

/// An output a start would add, for [`super::Studio::start_plan`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug)]
pub struct PlannedOutput<'a> {
    /// Its settings, as the start would pass them to `start_output`.
    pub settings: &'a OutputSettings,
    /// The video encoder the user picked, when one is: it is the only one tried.
    pub encoder: Option<&'a str>,
}

impl<'a> PlannedOutput<'a> {
    /// An output of `settings`, its encoder picked by `encoder` or by the selection.
    pub fn new(settings: &'a OutputSettings, encoder: Option<&'a str>) -> Self {
        Self { settings, encoder }
    }
}

/// What a start would add to what runs, for [`super::Studio::start_plan`] (ADR-0087
/// decision 1, spec `docs/specs/output-estimate.md#the-studios-plan`).
#[non_exhaustive]
#[derive(Clone, Debug, Default)]
pub struct StartPlanRequest<'a> {
    /// The outputs the start adds (a recording; with `canvas: "all"` one per canvas).
    pub outputs: Vec<PlannedOutput<'a>>,
    /// The separate video files and image tracks of the main recording, with their
    /// settings; `None` when the start writes no picture files.
    pub separate: Option<&'a IsoVideoStart>,
    /// The start writes the separate sound files of the sources marked for sound.
    pub sound_files: bool,
    /// Running outputs the start replaces (a recording that runs while the estimate is
    /// asked for its next start): their chains are left out unless another output
    /// uses them.
    pub replacing: Vec<OutputId>,
    /// The running separate files are left out (they belong to a replaced recording).
    pub replacing_separate: bool,
}

/// Bytes per second of 24-bit PCM at the graph's format: the separate sound files.
fn wav_bytes_per_second(format: rbs_core::media::AudioFormat) -> u64 {
    u64::from(format.sample_rate.hz()) * format.channels.channels() as u64 * 3
}

/// Bytes per second of an audio encoder: the bit rate, or PCM at `bit_depth` (16 when
/// unset) for a lossless one.
fn audio_bytes_per_second(a: &AudioSettings, format: rbs_core::media::AudioFormat) -> u64 {
    if a.bitrate_bps > 0 {
        return a.bitrate_bps / 8;
    }
    let depth = u64::from(a.bit_depth.unwrap_or(16));
    u64::from(format.sample_rate.hz()) * format.channels.channels() as u64 * depth / 8
}

impl super::Studio {
    /// What a start of the separate video files of `start` would run next to the
    /// outputs that already run, and what their adapters declare (ADR-0087 decision
    /// 1). The same as [`Self::start_plan`] with only `start`'s files added.
    pub fn separate_video_plan(
        &self,
        adapters: &RecordAdapters,
        collection: &SceneCollection,
        start: &IsoVideoStart,
    ) -> (EstimatePlan, Vec<DeclaredAdapter>) {
        let request = StartPlanRequest {
            separate: Some(start),
            ..StartPlanRequest::default()
        };
        self.start_plan(adapters, collection, &request)
    }

    /// The plan of a start (ADR-0087 decision 1): what runs now (encoder chains of
    /// every canvas once, and the running separate files) plus what `request` adds,
    /// and what their adapters declare. Reads only the studio's state and the
    /// registries' probe results: nothing opens. An added output shares a running
    /// chain when the start would (ADR-0047 decision 7); otherwise its video encoder
    /// is the one the user picked, else the one the selection would most likely use
    /// (the first candidate a running encoder uses, else the first candidate). The
    /// separate video files go to that likely encoder; marked sources without a
    /// picture yet are planned at the canvas size.
    pub fn start_plan(
        &self,
        adapters: &RecordAdapters,
        collection: &SceneCollection,
        request: &StartPlanRequest<'_>,
    ) -> (EstimatePlan, Vec<DeclaredAdapter>) {
        let fps = self.fps.per_second().as_f64();
        let graph_format = self.settings.audio_graph.format;
        let mut ids: Vec<rbs_core::registry::AdapterId> = Vec::new();
        let note = |ids: &mut Vec<rbs_core::registry::AdapterId>,
                    id: &rbs_core::registry::AdapterId| {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        };

        let mut video = Vec::new();
        for (slot, chain) in self.chains.video.iter().enumerate() {
            let Some(chain) = chain else { continue };
            if !self.slot_kept(slot, true, &request.replacing) {
                continue;
            }
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
        let mut audio: Vec<AudioEncode> = self
            .chains
            .audio
            .iter()
            .enumerate()
            .filter(|(slot, _)| self.slot_kept(*slot, false, &request.replacing))
            .filter_map(|(_, c)| c.as_ref())
            .map(|chain| AudioEncode {
                used_by: Use::Recording,
                bytes_per_second: audio_bytes_per_second(&chain.settings, graph_format),
            })
            .collect();
        if !request.replacing_separate {
            if let Some(run) = &self.iso_video {
                for e in &run.encodes {
                    if let Some(id) = adapters
                        .video_encoders
                        .ids()
                        .find(|id| id.as_str() == e.adapter)
                    {
                        note(&mut ids, id);
                    }
                    video.push(e.clone());
                }
            }
            if let Some(run) = &self.iso_audio {
                audio.extend(run.encodes.iter().cloned());
            }
        }

        for out in &request.outputs {
            self.plan_output(
                adapters, out, request, fps, &mut ids, &mut video, &mut audio,
            );
        }

        let marked = crate::iso::plan(collection)
            .map(|p| p.tracks)
            .unwrap_or_default();
        let mut images = Vec::new();
        let mut video_template = None;
        if let Some(start) = request.separate {
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
            video_template = iso_adapter.as_ref().map(|id| {
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
        }
        let mut audio_template = None;
        if request.sound_files {
            let wav = wav_bytes_per_second(graph_format);
            for t in marked.iter().filter(|t| t.role == IsoRole::Audio) {
                audio.push(AudioEncode {
                    used_by: Use::Iso(TrackRef {
                        source: t.source,
                        role: IsoRole::Audio,
                    }),
                    bytes_per_second: wav,
                });
            }
            audio_template = Some(AudioEncode {
                used_by: Use::Iso(TrackRef {
                    source: rbs_scene::model::SourceId::from_raw(0),
                    role: IsoRole::Audio,
                }),
                bytes_per_second: wav,
            });
        }
        let active = if request.separate.is_some() || request.sound_files {
            marked.len()
        } else {
            0
        };

        let give_way_share = request
            .separate
            .map_or(super::DEFAULT_GIVE_WAY_SHARE, |s| s.give_way_share);
        let plan = EstimatePlan {
            video,
            audio,
            images,
            active_sources: u32::try_from(active).unwrap_or(u32::MAX),
            inactive: Vec::new(),
            video_template,
            audio_template,
            iso_workers: SHARED_ENGINE_PARTITIONS,
            tick: self.fps.frame_duration(),
            give_way_share,
            planned_duration: DEFAULT_PLANNED_DURATION,
        };
        let declared = ids.iter().map(|id| declared(adapters, id)).collect();
        (plan, declared)
    }

    /// The running encoder chain in `slot` (video or audio) stays when a start
    /// replaces `replacing`: some other output uses it, or no output does (it drains).
    fn slot_kept(&self, slot: usize, video: bool, replacing: &[OutputId]) -> bool {
        let users: Vec<&super::Attached> = self
            .outputs
            .iter()
            .filter(|a| {
                if video {
                    a.video == Some(slot)
                } else {
                    a.audio == Some(slot)
                }
            })
            .collect();
        users.is_empty() || users.iter().any(|a| !replacing.contains(&a.id))
    }

    /// Adds what output `out` would open to `video` and `audio`.
    #[allow(clippy::too_many_arguments)]
    fn plan_output(
        &self,
        adapters: &RecordAdapters,
        out: &PlannedOutput<'_>,
        request: &StartPlanRequest<'_>,
        fps: f64,
        ids: &mut Vec<rbs_core::registry::AdapterId>,
        video: &mut Vec<VideoEncode>,
        audio: &mut Vec<AudioEncode>,
    ) {
        let s = out.settings;
        let canvas = s.canvas.unwrap_or(self.settings.canvas);
        if let Some(v) = &s.video {
            let canvas_size = self.canvas_size(canvas).unwrap_or(self.size);
            let size = super::resolve_output_size(s.size, canvas_size).unwrap_or(canvas_size);
            let config = super::video_encoder_config(
                v,
                VideoFormat::new(Size::new(size.0, size.1), v.format.pixel_format(), v.color),
                self.fps,
            );
            let shared = self.chains.video.iter().enumerate().any(|(slot, c)| {
                c.as_ref().is_some_and(|c| {
                    self.slot_kept(slot, true, &request.replacing)
                        && c.canvas == canvas
                        && c.settings == *v
                        && c.size == size
                        && c.end.requested().is_none()
                        && select::is_video_candidate(
                            &adapters.video_encoders,
                            &config,
                            &c.chosen.adapter,
                        )
                })
            });
            if !shared {
                let adapter = match out.encoder {
                    Some(picked) => adapters
                        .video_encoders
                        .ids()
                        .find(|id| id.as_str() == picked)
                        .cloned(),
                    None => select::likely_video_encoder(&adapters.video_encoders, &config, ids),
                };
                if let Some(id) = adapter {
                    if !ids.contains(&id) {
                        ids.push(id.clone());
                    }
                    video.push(self.encode_of(adapters, Use::Recording, id.as_str(), v, size, fps));
                }
            }
        }
        if let Some(a) = &s.audio {
            let config = a.encoder_config(self.settings.audio_graph.format);
            let shared = self.chains.audio.iter().enumerate().any(|(slot, c)| {
                c.as_ref().is_some_and(|c| {
                    self.slot_kept(slot, false, &request.replacing)
                        && c.settings == *a
                        && select::is_audio_candidate(
                            &adapters.audio_encoders,
                            &config,
                            &c.chosen.adapter,
                        )
                })
            });
            if !shared {
                audio.push(AudioEncode {
                    used_by: Use::Recording,
                    bytes_per_second: audio_bytes_per_second(a, self.settings.audio_graph.format),
                });
            }
        }
    }

    pub(super) fn encode_of(
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
            format: settings.format.into(),
        }
    }
}

impl super::Studio {
    /// What a calibration run for `request` measures (ADR-0087 decision 4, spec
    /// `output-estimate.md` "Calibration run"): the classes of the plan
    /// [`Self::start_plan`] builds, the master's first, each at the largest planned
    /// size of its class, with every candidate the selection would consider. Reads
    /// only the studio's state and the registries; nothing opens. `folder` is the
    /// profile's recording folder for the canvas.
    pub fn calibration_plan(
        &self,
        adapters: &RecordAdapters,
        collection: &SceneCollection,
        request: &StartPlanRequest<'_>,
        folder: std::path::PathBuf,
    ) -> crate::calibration::CalibrationPlan {
        use crate::calibration::{Candidate, ClassKey, ClassPlan};

        let (plan, _) = self.start_plan(adapters, collection, request);
        let master = request
            .outputs
            .iter()
            .find_map(|o| o.settings.video.as_ref().map(|v| (v, o.encoder)));
        struct Class<'s> {
            key: ClassKey,
            settings: &'s VideoSettings,
            size: (u32, u32),
            picked: Option<&'s str>,
            likely: String,
        }
        let mut classes: Vec<Class<'_>> = Vec::new();
        for e in &plan.video {
            let (settings, picked) = match e.used_by {
                Use::Recording => match master {
                    Some((v, picked)) => (v, picked),
                    None => continue,
                },
                Use::Iso(_) => match request.separate {
                    Some(s) => (&s.video, None),
                    None => continue,
                },
                // A calibration plans a recording; nothing streams while it runs.
                Use::Stream => continue,
            };
            let (size, fps, rate) = e.class();
            let key = ClassKey {
                codec: e.codec.clone(),
                size,
                fps,
                rate,
            };
            match classes.iter_mut().find(|c| c.key == key) {
                Some(c) => {
                    let pixels = |s: (u32, u32)| u64::from(s.0) * u64::from(s.1);
                    if pixels((e.width, e.height)) > pixels(c.size) {
                        c.size = (e.width, e.height);
                    }
                    // A class served by a picked encoder and by the selection is
                    // measured on every candidate.
                    if picked.is_none() {
                        c.picked = None;
                    }
                }
                None => classes.push(Class {
                    key,
                    settings,
                    size: (e.width, e.height),
                    picked,
                    likely: e.adapter.clone(),
                }),
            }
        }
        let classes = classes
            .into_iter()
            .map(|c| {
                let Class {
                    key,
                    settings,
                    size,
                    picked,
                    likely,
                } = c;
                let config = super::video_encoder_config(
                    settings,
                    VideoFormat::new(
                        Size::new(size.0, size.1),
                        settings.format.pixel_format(),
                        settings.color,
                    ),
                    self.fps,
                );
                let candidates = select::video_candidates(&adapters.video_encoders, &config)
                    .into_iter()
                    .filter(|f| picked.is_none_or(|p| f.id().as_str() == p))
                    .map(|f| {
                        let id = f.id();
                        Candidate {
                            version: None,
                            hardware: declared(adapters, &id).hardware,
                            likely: id.as_str() == likely,
                            adapter: id,
                            factory: f,
                        }
                    })
                    .collect();
                ClassPlan {
                    key,
                    config,
                    candidates,
                }
            })
            .collect();
        // The export classes of the separate video files (step D2): one per size
        // class, at its bound size or the largest planned size, largest first.
        let mut exports: Vec<crate::calibration::ExportClass> = Vec::new();
        if let Some(s) = request.separate {
            for e in plan
                .video
                .iter()
                .filter(|e| matches!(e.used_by, Use::Iso(_)))
            {
                let size = crate::estimate::SizeClass::of(e.height);
                let (width, height) =
                    crate::calibration::ExportClass::bound(size, (e.width, e.height));
                match exports.iter_mut().find(|x| x.size == size) {
                    Some(x) => {
                        if u64::from(width) * u64::from(height)
                            > u64::from(x.width) * u64::from(x.height)
                        {
                            (x.width, x.height) = (width, height);
                        }
                    }
                    None => exports.push(crate::calibration::ExportClass {
                        size,
                        width,
                        height,
                        format: s.video.format,
                        color: s.video.color,
                        open_shadows: s.video.open_shadows,
                    }),
                }
            }
        }
        exports.sort_by_key(|x| std::cmp::Reverse(u64::from(x.width) * u64::from(x.height)));
        crate::calibration::CalibrationPlan {
            classes,
            folder,
            exports,
            frame_rate: self.fps,
        }
    }
}
