//! The PNG writer of an image track (ADR-0117 decision 5).
//!
//! [`PngWriterNode`] runs on [`crate::ISO_OUTPUT`], never on the render thread. Each new
//! frame on its input (a straight-alpha sRGB `Rgba8` system frame from
//! `rbs_nodes::ImageTrackNode`) is encoded with the `png` crate and written into the
//! take folder: `<stem>.png` first, then `<stem>-2.png`, `<stem>-3.png`, ... A file is
//! written under a temporary name and renamed, so a crash never leaves half an image
//! under its final name. The files are written directly, as the manifest is, not
//! through a transport (owner's choice, 2026-11-14; ADR-0117 `## Review`).

use std::fs;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rbs_core::media::{FramePayload, PixelFormat, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};
use rbs_manifest::ImageEntry;

/// Type id of [`PngWriterNode`].
pub const PNG_WRITER_NODE_TYPE: &str = "org.rbs.png-writer";

const VIDEO_PIN: usize = 0;

/// What a writer wrote, readable from the control thread.
#[derive(Debug, Default)]
pub struct ImageFiles {
    entries: Mutex<Vec<ImageEntry>>,
    /// Pictures that could not be encoded or written.
    pub failed: AtomicU64,
}

impl ImageFiles {
    /// The images written so far, in order.
    pub fn entries(&self) -> Vec<ImageEntry> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Encodes and writes the pictures of one image track.
pub struct PngWriterNode {
    descriptor: NodeDescriptor,
    dir: PathBuf,
    stem: String,
    t0: Timestamp,
    /// The number of the next image (1 is `<stem>.png`).
    next: u32,
    last: Option<(Timestamp, u64)>,
    files: Arc<ImageFiles>,
}

impl PngWriterNode {
    /// A writer of `<stem>.png`, `<stem>-2.png`, ... into `dir`, timing each image from
    /// `t0`. `stem` is a single file name without extension.
    pub fn new(dir: PathBuf, stem: &str, t0: Timestamp) -> Self {
        Self {
            descriptor: NodeDescriptor::new(
                NodeTypeId::new_static(PNG_WRITER_NODE_TYPE),
                NodeKind::Output,
                vec![Pin::one("video", LinkKind::Video)],
                vec![],
            ),
            dir,
            stem: stem.to_owned(),
            t0,
            next: 1,
            last: None,
            files: Arc::default(),
        }
    }

    /// The images written and the failures.
    pub fn files(&self) -> Arc<ImageFiles> {
        Arc::clone(&self.files)
    }

    /// The file name of image `n` of `stem`.
    pub fn file_name(stem: &str, n: u32) -> String {
        if n <= 1 {
            format!("{stem}.png")
        } else {
            format!("{stem}-{n}.png")
        }
    }

    fn write(&mut self, frame: &VideoFrame) -> Result<(), &'static str> {
        let FramePayload::System(sys) = frame.payload() else {
            return Err("image writer needs a system frame");
        };
        let format = frame.format();
        if format.pixel_format != PixelFormat::Rgba8 {
            return Err("image writer needs Rgba8");
        }
        let (width, height) = (format.size.width, format.size.height);
        let (bytes, stride) = sys.plane(0).ok_or("image frame has no plane")?;
        let row = width as usize * 4;
        let name = Self::file_name(&self.stem, self.next);
        let temp = self.dir.join(format!(".{name}.part"));
        let encoded = (|| -> Result<(), png::EncodingError> {
            let file = fs::File::create(&temp)?;
            let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
            let mut writer = encoder.write_header()?;
            let mut stream = writer.stream_writer()?;
            for y in 0..height as usize {
                std::io::Write::write_all(&mut stream, &bytes[y * stride..y * stride + row])?;
            }
            stream.finish()?;
            Ok(())
        })();
        if encoded.is_err() {
            let _ = fs::remove_file(&temp);
            return Err("image could not be encoded or written");
        }
        if fs::rename(&temp, self.dir.join(&name)).is_err() {
            let _ = fs::remove_file(&temp);
            return Err("image could not be renamed");
        }
        self.next += 1;
        let at_ns = frame
            .timestamp()
            .as_nanos()
            .saturating_sub(self.t0.as_nanos());
        self.files
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(ImageEntry {
                file_name: name,
                width,
                height,
                at_ns,
            });
        Ok(())
    }
}

impl<X: Runtime> Node<X> for PngWriterNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    /// Not real time: runs on the ISO output worker, encodes and writes a file per new
    /// picture (a few per second at most, ADR-0117 decision 3).
    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let Some(frame) = ctx.inputs().video(VIDEO_PIN, 0) else {
            return Ok(());
        };
        let key = (frame.timestamp(), frame.sequence());
        if self.last == Some(key) {
            return Ok(());
        }
        self.last = Some(key);
        self.write(frame).map_err(|e| {
            self.files.failed.fetch_add(1, Ordering::Relaxed);
            NodeError::Backend(e)
        })
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, X>) {}
}
