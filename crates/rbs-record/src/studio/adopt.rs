//! Adopting text and slide show instances into the render partition, and the
//! slide show calls of the studio (ADR-0129, ADR-0171). Both hand their node's picture
//! sender to a control-side worker: the text worker or the slide show hub.

use std::sync::Arc;

use rbs_engine::Engine;
use rbs_gpu::node::GpuRuntime;
use rbs_scene::model::SourceId;
use rbs_sources::SourceTypes;
use rbs_sources::slideshow::{SlideCommand, SlideState, SlideshowHub, SlideshowSource};
use rbs_sources::text_file::FileProbe;
use rbs_sources::text_live::LiveInputs;
use rbs_sources::text_worker::{SystemTextClock, TextClock, TextWorker};

use super::{Entry, Instance, SoundHalf, Studio, Watch};
use crate::{RENDER, RecordError};

/// The text worker, made on first use with the registered renderers.
pub(super) fn text_worker<'a>(
    text: &'a mut Option<TextWorker>,
    io: &(Arc<dyn FileProbe>, Option<Arc<dyn TextClock>>),
    clock: Arc<dyn rbs_core::time::Clock>,
    sources: &SourceTypes,
    live: &LiveInputs,
) -> &'a mut TextWorker {
    text.get_or_insert_with(|| {
        let mut worker = TextWorker::new(
            sources.text_renderers(),
            Arc::clone(&io.0),
            clock,
            io.1.clone()
                .or_else(|| sources.text_clock())
                .unwrap_or_else(|| Arc::new(SystemTextClock)),
        );
        worker.set_live(live);
        worker
    })
}

/// The render-partition instance record of a picture-only source.
pub(super) fn render_instance(watch: Watch) -> Instance {
    Instance {
        partition: RENDER,
        gpu: true,
        watch,
        offset: None,
        sent_offset_ns: 0,
        sound: SoundHalf::None,
    }
}

/// Adopts a slide show's node and attaches its sender to the hub (ADR-0171 decision 8).
pub(super) fn adopt_slideshow(
    engine: &mut Engine,
    hub: &mut SlideshowHub,
    entry: &mut Entry,
    s: SlideshowSource,
) -> Result<(), RecordError> {
    let size = s.size;
    engine.adopt::<GpuRuntime>(RENDER, entry.state.node, Box::new(s.node))?;
    hub.attach(entry.source, s.settings, s.size, s.shared, s.frames);
    entry.instance = Some(render_instance(Watch::Sized { size }));
    Ok(())
}

impl Studio {
    /// Runs a slide show command (`media.next`, `media.previous`, `media.restart`);
    /// false when `source` is not a running slide show.
    pub fn slideshow_command(&self, source: SourceId, command: SlideCommand) -> bool {
        self.slideshows.command(source, command)
    }

    /// Pauses or resumes a slide show's timer; false when it is not running.
    pub fn set_slideshow_paused(&self, source: SourceId, paused: bool) -> bool {
        self.slideshows.set_paused(source, paused)
    }

    /// What `media.state` reports for a running slide show.
    pub fn slideshow_state(&self, source: SourceId) -> Option<SlideState> {
        self.slideshows.state(source)
    }
}

/// Live slide show keys go to the hub; the node stays (ADR-0171 decision 4).
pub(super) fn slideshow_live(
    hub: &mut SlideshowHub,
    collection: &rbs_scene::SceneCollection,
    source: SourceId,
) {
    if let Some(s) = collection.source(source)
        && hub.has(source)
    {
        hub.set_settings(source, rbs_sources::slideshow::read_settings(&s.settings));
    }
}
