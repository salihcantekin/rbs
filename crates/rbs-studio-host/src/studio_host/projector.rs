//! Projectors: their canvases, opening, closing and keeping them in step with the studio (ADR-0159 decision 8).

use super::*;

impl Host {
    /// The canvas a projector shows and its size, while that canvas runs.
    pub(super) fn projector_canvas(
        &self,
        canvas: OutputCanvas,
    ) -> Option<(CanvasId, Size, FrameRate)> {
        let id = match canvas {
            OutputCanvas::Main => self.canvas,
            OutputCanvas::Vertical => self.verticals.vertical?.canvas,
        };
        let info = self.collection.canvas(id)?;
        Some((id, info.size, info.frame_rate))
    }

    /// Opens projector `id` (ADR-0125): refused beyond the limits of decision 4 with
    /// [`ProjectorState::Closed`]; its picture is added by [`Self::sync_projectors`].
    pub(super) fn open_projector(
        &mut self,
        id: u32,
        canvas: OutputCanvas,
        notify: &dyn Fn(Update),
    ) {
        self.close_projector(id);
        if self.projectors.iter().any(|p| p.id == id) {
            notify(Update::Error(
                "The projector could not be replaced while a Program update is pending.".to_owned(),
            ));
            return;
        }
        if let Some((_, size, _)) = self.projector_canvas(canvas) {
            let (count, open) = self.projectors_open();
            if let Some(why) = projector_refusal(count, open, size) {
                notify(Update::Projector {
                    id,
                    picture: None,
                    state: ProjectorState::Closed(why.to_owned()),
                });
                return;
            }
        }
        self.projectors.push(ProjectorRun {
            id,
            canvas,
            picture: None,
            closing: false,
            size: Size::new(0, 0),
            sent: None,
        });
    }

    /// Closes projector `id`: its picture leaves the studio.
    pub(super) fn close_projector(&mut self, id: u32) {
        let Some(at) = self.projectors.iter().position(|p| p.id == id) else {
            return;
        };
        let run = if self.projectors[at].picture.is_some() {
            let Ok(run) = self.auxiliary_run_collection() else {
                self.projectors[at].closing = true;
                return;
            };
            run
        } else {
            None
        };
        if let Some(picture) = self.projectors[at].picture {
            let running = run.as_ref().unwrap_or(&self.collection);
            let result = self
                .studio
                .remove_picture(running, &self.adapters.sources, picture);
            self.waiting = self.studio.waiting();
            if result.is_err() && self.studio.pictures().contains(&picture) {
                self.projectors[at].closing = true;
                return;
            }
        }
        self.projectors.remove(at);
    }

    /// How many projectors have a picture, and the surface memory they count.
    pub(super) fn projectors_open(&self) -> (usize, u64) {
        self.projectors
            .iter()
            .filter(|p| p.picture.is_some())
            .fold((0, 0), |(n, b), p| {
                (n + 1, b + projector_surface_bytes(p.size))
            })
    }

    /// Brings the projectors' pictures in line with the studio (ADR-0125 decision 3):
    /// a picture that failed is removed and its projector closed; a projector whose
    /// canvas runs and that has no picture gets one (after a restart, or when the
    /// vertical canvas comes back); one whose canvas is gone is suspended. Tells the UI
    /// what changed. Nothing runs while there is no projector (P13).
    pub(super) fn sync_projectors(&mut self, notify: &dyn Fn(Update)) {
        if self.projectors.is_empty() {
            return;
        }
        let closing: Vec<_> = self
            .projectors
            .iter()
            .filter(|p| p.closing)
            .map(|p| p.id)
            .collect();
        for id in closing {
            self.close_projector(id);
        }
        let present = self.studio.pictures();
        let mut closed = Vec::new();
        for at in 0..self.projectors.len() {
            if self.projectors[at].closing {
                continue;
            }
            let (canvas, picture) = (self.projectors[at].canvas, self.projectors[at].picture);
            // Gone with its canvas (the studio removed it).
            if let Some(pid) = picture
                && !present.contains(&pid)
            {
                self.projectors[at].picture = None;
            }
            if let Some(pid) = self.projectors[at].picture
                && let Some(why) = self.studio.picture_failure(pid).map(str::to_owned)
            {
                closed.push((
                    self.projectors[at].id,
                    format!("The projector stopped: {why}"),
                ));
                continue;
            }
            if self.projectors[at].picture.is_some() {
                continue;
            }
            let Some((canvas_id, size, fps)) = self.projector_canvas(canvas) else {
                continue;
            };
            let (count, open) = self.projectors_open();
            if let Some(why) = projector_refusal(count, open, size) {
                closed.push((self.projectors[at].id, why.to_owned()));
                continue;
            }
            let desc = PreviewDesc {
                width: size.width,
                height: size.height,
                surfaces: preview_surfaces(fps),
            };
            let Ok(run) = self.auxiliary_run_collection() else {
                continue;
            };
            let running = run.as_ref().unwrap_or(&self.collection);
            match self
                .studio
                .add_picture(running, &self.adapters.sources, canvas_id, desc)
            {
                Ok((pid, _)) => {
                    self.projectors[at].picture = Some(pid);
                    self.projectors[at].size = size;
                }
                Err(e) => closed.push((
                    self.projectors[at].id,
                    format!("The projector did not start: {e}"),
                )),
            }
            self.waiting = self.studio.waiting();
        }
        for (id, why) in closed {
            self.close_projector(id);
            notify(Update::Projector {
                id,
                picture: None,
                state: ProjectorState::Closed(why.clone()),
            });
            notify(Update::Error(why));
        }
        for p in &mut self.projectors {
            if p.closing {
                continue;
            }
            let now = match p.picture {
                Some(pid) => (self.studio.picture(pid), ProjectorState::Open),
                None => (None, ProjectorState::Suspended),
            };
            if p.sent.as_ref() != Some(&now) {
                p.sent = Some(now.clone());
                notify(Update::Projector {
                    id: p.id,
                    picture: now.0,
                    state: now.1,
                });
            }
        }
    }
}
