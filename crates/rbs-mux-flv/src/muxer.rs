//! The FLV muxer session.

use rbs_core::media::{Bytes, ChunkKind, EncodedPacket, MuxChunk, StreamIndex};
use rbs_core::time::Timestamp;
use rbs_media_format::flv::{
    self, ConfigError, FlvTracks, PacketError, TAG_AUDIO, TAG_HEADER_LEN, TAG_SCRIPT, TAG_VIDEO,
    TrackKind,
};
use rbs_mux::{CreateError, MuxError, Muxer, MuxerConfig};

pub(crate) struct FlvMuxer {
    tracks: FlvTracks,
    last_dts: Vec<Option<Timestamp>>,
    /// Master time of FLV timestamp 0: the first dts written.
    origin: Option<Timestamp>,
    last_chunk: Option<Timestamp>,
    /// How far before the first dts the FLV time origin lies, so streams that arrive
    /// slightly out of order across streams keep their timestamps.
    origin_lead: std::time::Duration,
    began: bool,
    finished: bool,
}

impl FlvMuxer {
    pub(crate) fn new(config: &MuxerConfig) -> Result<Self, CreateError> {
        let tracks = FlvTracks::new(&config.streams).map_err(|e| match e {
            ConfigError::Unsupported(what) => CreateError::Unsupported { what },
            ConfigError::Invalid(what) => CreateError::InvalidConfig(what),
        })?;
        Ok(Self {
            tracks,
            last_dts: vec![None; config.streams.len()],
            origin: None,
            last_chunk: None,
            origin_lead: config.max_buffered(),
            began: false,
            finished: false,
        })
    }
}

fn packet_error(stream: StreamIndex, e: PacketError) -> MuxError {
    match e {
        PacketError::UnknownStream => MuxError::UnknownStream(stream.0),
        other => MuxError::InvalidPacket(other.to_string()),
    }
}

impl Muxer for FlvMuxer {
    fn begin(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if self.began {
            return Err(MuxError::InvalidState("begin called twice"));
        }
        self.began = true;
        let tracks = &self.tracks;
        let flags = (u8::from(tracks.has_audio()) << 2) | u8::from(tracks.has_video());
        let mut header = vec![b'F', b'L', b'V', 1, flags, 0, 0, 0, 9, 0, 0, 0, 0];
        let mut body = Vec::new();
        tracks.metadata_body(&mut body);
        flv::write_tag(&mut header, TAG_SCRIPT, 0, &body);
        body.clear();
        if tracks.video_sequence_header(&mut body) {
            flv::write_tag(&mut header, TAG_VIDEO, 0, &body);
        }
        body.clear();
        if tracks.audio_sequence_header(&mut body) {
            flv::write_tag(&mut header, TAG_AUDIO, 0, &body);
        }
        out.push(MuxChunk::new(ChunkKind::Header, Bytes::from(header)));
        Ok(())
    }

    fn write(
        &mut self,
        stream: StreamIndex,
        packet: &EncodedPacket,
        out: &mut Vec<MuxChunk>,
    ) -> Result<(), MuxError> {
        if !self.began || self.finished {
            return Err(MuxError::InvalidState("write outside begin..finish"));
        }
        let index = stream.get();
        let plan = self
            .tracks
            .plan(index, packet)
            .map_err(|e| packet_error(stream, e))?;
        let dts = packet.dts();
        if self.last_dts[index].is_some_and(|last| dts < last) {
            return Err(MuxError::NonMonotonic { stream: stream.0 });
        }
        let lead = self.origin_lead;
        let origin = *self.origin.get_or_insert(dts - lead);
        if dts < origin {
            return Err(MuxError::InvalidPacket(
                "dts more than the interleaving bound before the first packet".into(),
            ));
        }
        let dts_ms = flv::millis_since(dts, origin);
        // FLV time is 32-bit milliseconds; it wraps after 49.7 days, which RTMP
        // servers and players handle.
        let ms = dts_ms as u32;
        // Composition time from the same rounded clock as the dts.
        let cts = flv::millis_since(packet.pts(), origin) - dts_ms;
        self.last_dts[index] = Some(dts);

        let keyframe = match plan.kind {
            TrackKind::Video => packet.keyframe(),
            TrackKind::Audio => !self.tracks.has_video(),
        };
        // One chunk per tag, sized exactly: a transport may drop or refuse whole tags
        // only, and the chunk is the only allocation for this packet.
        let mut tag = Vec::with_capacity(TAG_HEADER_LEN + plan.data_size + 4);
        tag.extend_from_slice(&flv::tag_header(plan.tag_type(), plan.data_size, ms));
        self.tracks.write_body(plan, packet, cts, &mut tag);
        flv::previous_tag_size(&mut tag, plan.data_size);
        debug_assert_eq!(tag.len(), tag.capacity());

        let timestamp = self.last_chunk.map_or(dts, |last| last.max(dts));
        self.last_chunk = Some(timestamp);
        out.push(MuxChunk::new(
            ChunkKind::Media {
                keyframe,
                timestamp,
            },
            Bytes::from(tag),
        ));
        Ok(())
    }

    fn finish(&mut self, _out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if !self.began || self.finished {
            return Err(MuxError::InvalidState("finish outside begin..finish"));
        }
        // Live FLV: no trailer; duration and file size stay 0 in the metadata.
        self.finished = true;
        Ok(())
    }
}
