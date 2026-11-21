//! The window's handle on the control thread (StudioHost) and the control API it serves.

use super::*;

/// The running control thread. Dropping it stops the studio and joins the thread.
pub struct StudioHost {
    pub(super) tx: Option<Arc<ControlOut>>,
    pub(super) thread: Option<JoinHandle<()>>,
    /// Meters the control API samples for `audio.levels`.
    pub(super) levels: Arc<crate::levels::LevelFeed>,
}

impl StudioHost {
    /// Starts the control thread. It makes the adapters (`adapters`, then probes them)
    /// and the GPU context (`gpu`) on that thread, starts the studio with an empty scene
    /// and reports through `notify`, which runs on the control thread and must not block.
    /// `engine` makes the studio's engine, again whenever the canvas changes.
    pub fn spawn(
        config: HostConfig,
        adapters: impl FnOnce() -> (RecordAdapters, Vec<String>) + Send + 'static,
        gpu: impl FnOnce() -> Result<Arc<GpuContext>, String> + Send + 'static,
        engine: impl Fn() -> Engine + Send + 'static,
        notify: impl Fn(Update) + Send + 'static,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let tx = ControlOut::new(tx);
        let levels = crate::levels::LevelFeed::new();
        let thread = std::thread::Builder::new()
            .name("rbs-studio".into())
            .spawn({
                let inbox = Arc::downgrade(&tx);
                let levels = Arc::clone(&levels);
                move || {
                    run(
                        config,
                        adapters,
                        gpu,
                        engine,
                        ControlIo { rx, inbox, levels },
                        &notify,
                    )
                }
            })?;
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
            levels,
        })
    }

    /// The meters `audio.levels` samples while remote control is on.
    pub fn level_feed(&self) -> Arc<crate::levels::LevelFeed> {
        Arc::clone(&self.levels)
    }

    /// Queues a command. Never blocks; a stopped studio ignores it.
    pub fn send(&self, c: Command) {
        self.post(Msg::Ui(c));
    }

    /// Serves the control API on the control thread from now on: `endpoint` comes from
    /// `rbs_api::ApiServer::start`, whose control waker is [`StudioHost::api_waker`].
    /// With `tokens`, `tokens.*` requests are answered (local peers only, by scope);
    /// without, they fail.
    pub fn attach_api(&self, endpoint: ControlEndpoint, tokens: Option<ApiTokens>) {
        self.post(Msg::AttachApi(Box::new(Api { endpoint, tokens })));
    }

    /// Stops serving the control API (remote control turned off).
    pub fn detach_api(&self) {
        self.post(Msg::DetachApi);
    }

    /// Wakes the control thread when the API forwarded a request. Never blocks.
    /// It holds no strong reference to the channel, so dropping the host still closes
    /// it and ends the control thread while an `ApiServer` keeps the waker.
    pub fn api_waker(&self) -> impl Fn() + Send + Sync + 'static {
        let tx: std::sync::Weak<ControlOut> =
            self.tx.as_ref().map(Arc::downgrade).unwrap_or_default();
        move || {
            if let Some(tx) = tx.upgrade() {
                let _ = tx.send(Msg::ApiWake);
            }
        }
    }

    pub(super) fn post(&self, m: Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(m);
        }
    }
}

impl Drop for StudioHost {
    fn drop(&mut self) {
        crate::ui_thread::assert_off_ui("stopping the studio host"); // Joins (ADR-0156).
        // The last `ControlOut` disconnects and unparks the parked control thread, here
        // or on a worker that still holds it.
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The control API while it is attached.
pub(super) struct Api {
    pub(super) endpoint: ControlEndpoint,
    pub(super) tokens: Option<ApiTokens>,
}

impl Api {
    /// Serves one request: `tokens.*` from the settings, everything else by `target`.
    pub(super) fn serve_one(
        &self,
        target: &mut dyn ControlTarget,
        events: &mut EventPublisher,
        notify: &dyn Fn(Update),
    ) -> bool {
        let mut with_tokens = WithTokens {
            inner: target,
            tokens: self.tokens.as_ref(),
            notify,
        };
        self.endpoint.serve_one(&mut with_tokens, events)
    }
}

/// Answers `tokens.*` (they do not touch the studio, so they work also after it
/// failed), and passes everything else on.
pub(super) struct WithTokens<'a> {
    pub(super) inner: &'a mut dyn ControlTarget,
    pub(super) tokens: Option<&'a ApiTokens>,
    pub(super) notify: &'a dyn Fn(Update),
}

impl ControlTarget for WithTokens<'_> {
    fn handle_as(
        &mut self,
        caller: &rbs_api::Caller,
        command: rbs_api::Command,
        events: &mut EventPublisher,
    ) -> Result<Response, ApiError> {
        if let Some(answer) = self.tokens.and_then(|t| t.answer(&command)) {
            let (response, changed) = answer?;
            if changed {
                (self.notify)(Update::TokensChanged);
            }
            return Ok(response);
        }
        self.inner.handle_as(caller, command, events)
    }

    fn handle(
        &mut self,
        command: rbs_api::Command,
        events: &mut EventPublisher,
    ) -> Result<Response, ApiError> {
        if let Some(answer) = self.tokens.and_then(|t| t.answer(&command)) {
            let (response, changed) = answer?;
            if changed {
                (self.notify)(Update::TokensChanged);
            }
            return Ok(response);
        }
        self.inner.handle(command, events)
    }
}
