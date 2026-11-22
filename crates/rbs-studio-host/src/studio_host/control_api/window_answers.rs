//! What the window waits for after its own control API calls (ADR-0168 decision 2):
//! the state update its own command sent, also after a refusal. Clients get only the
//! answer.

use super::*;

impl ApiTarget<'_> {
    /// Runs the window's `command` and sends the update it waits for.
    pub(super) fn run_for_window(
        &mut self,
        command: rbs_api::Command,
    ) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        match command {
            // Settings, Video closed its preview first: it waits for the canvas, also
            // when nothing changed or the change was refused.
            C::SettingsSet(ref p) if p.canvas.is_some() => {
                let told = self.host.canvas_told;
                let result = self.route(command);
                if self.host.canvas_told == told {
                    self.host.tell_canvas(notify);
                }
                result
            }
            // The camera's card waits for its state, also when nothing sent.
            C::VcamStop => {
                let result = self.route(command);
                if result.is_err() {
                    notify(Update::VirtualCamera(self.host.camera_status()));
                }
                result
            }
            // The replay notice waits for the save; the worker tells a written clip.
            C::ReplaySave(_) => {
                let result = self.route(command);
                if let Err(e) = &result {
                    notify(Update::ReplaySaved(Err(e.message.clone())));
                }
                result
            }
            // The Measure view's phase stays on the host.
            C::SyncMeasureStart(ref p) => {
                let (source, reference) = (
                    SourceId::from_raw(p.source),
                    p.reference.map(SourceId::from_raw),
                );
                let result = self.route(command);
                let ended = result.as_ref().map(|_| ());
                self.host
                    .measure_view_started(source, reference, ended, notify);
                result
            }
            command => self.route(command),
        }
    }
}
