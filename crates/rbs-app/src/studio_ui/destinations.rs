//! Settings, Stream: the Destinations card and its inline editor (ADR-0127 decision
//! 17). The list is the picked canvas's destinations, its first one from the stream
//! settings; the editor works on a copy, written on Save. Keys are never shown: they go
//! to the system's credential store at once, as the Stream key field always did.

use std::time::Instant;

use rbs_api::types::{DestinationState, LiveDestination, OutputCanvas};
use rbs_app::record_profile::Encoding;
use rbs_app::stream_destinations::{
    CUSTOM, Canvas, Destination, DestinationEncoding, Destinations, MAX_DESTINATIONS_PER_CANVAS,
    Resolved,
};
use rbs_app::stream_profile::{StreamProfile, parse_server};
use rbs_app::stream_services::{self, Service};

/// The canvas a Settings canvas picks.
pub(crate) fn role(c: OutputCanvas) -> Canvas {
    match c {
        OutputCanvas::Main => Canvas::Main,
        OutputCanvas::Vertical => Canvas::Vertical,
    }
}

/// One row of the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub id: u32,
    pub name: String,
    pub note: String,
    pub on: bool,
    /// Something is missing (a server or a key): the note says what.
    pub warn: bool,
}

/// The service a destination shows: its saved one, else the one its server belongs to.
pub(crate) fn service_of(d: &Destination) -> Option<&'static Service> {
    stream_services::get(&d.service)
        .filter(|s| s.id != CUSTOM)
        .or_else(|| stream_services::by_server(&d.server))
}

/// The name a row shows: the saved one, the service's, else "Destination <n>".
pub(crate) fn label(d: &Destination) -> String {
    if !d.name.trim().is_empty() {
        return d.name.clone();
    }
    match service_of(d) {
        Some(s) => s.name.clone(),
        None if d.is_first() => "Stream".to_owned(),
        None => format!("Destination {}", d.id),
    }
}

/// What a row says under the name: service, protocol, key and encoding.
fn note(r: &Resolved, stream: &Encoding) -> (String, bool) {
    let d = &r.destination;
    let mut parts = Vec::new();
    if let Some(s) = service_of(d) {
        parts.push(s.name.clone());
    }
    let parsed = r.parsed();
    let mut warn = false;
    match &parsed {
        None => {
            parts.push("No server yet".to_owned());
            warn = true;
        }
        Some(server) => {
            parts.push(server.protocol.label().to_owned());
            if server.protocol.needs_key() {
                if d.key_saved {
                    parts.push("Key saved".to_owned());
                } else {
                    parts.push("No key yet".to_owned());
                    warn = true;
                }
            }
        }
    }
    parts.push(match &d.encoding {
        DestinationEncoding::Shared => "Same as the stream".to_owned(),
        DestinationEncoding::Own { encoding, .. } => {
            let _ = stream;
            format!("Own settings: {}", encoding.video_summary())
        }
    });
    (parts.join(" · "), warn)
}

/// The rows of `canvas`, its first destination first.
pub(crate) fn rows(all: &Destinations, canvas: Canvas, stream: &StreamProfile) -> Vec<Row> {
    all.of(canvas, stream, "")
        .iter()
        .map(|r| {
            let (note, warn) = note(r, &stream.encoding);
            Row {
                id: r.destination.id,
                name: label(&r.destination),
                note,
                on: r.destination.enabled,
                warn,
            }
        })
        .collect()
}

/// What the card says under the list: the upload Go live needs and each service's
/// limit the settings break. Only enabled destinations with a server count.
pub(crate) fn notes(
    all: &Destinations,
    canvas: Canvas,
    stream: &StreamProfile,
) -> (String, Vec<String>) {
    let used: Vec<Resolved> = all
        .of(canvas, stream, "")
        .into_iter()
        .filter(|r| r.destination.enabled && r.parsed().is_some())
        .collect();
    let kbps = |e: &Encoding| u64::from(e.peak_kbps()) + u64::from(e.audio_kbps);
    let mut total = 0_u64;
    let mut own = 0_usize;
    let mut warnings = Vec::new();
    for r in &used {
        let e = match &r.destination.encoding {
            DestinationEncoding::Shared => &stream.encoding,
            DestinationEncoding::Own { encoding, .. } => {
                own += 1;
                encoding
            }
        };
        total += kbps(e);
        if let Some(s) = service_of(&r.destination) {
            warnings.extend(stream_services::warnings(s, e));
        }
    }
    let upload = match used.len() {
        0 | 1 => String::new(),
        n => {
            let mbps = total.div_ceil(1000);
            let shared = n - own;
            let how = match (shared, own) {
                (_, 0) => "one encode shared by all".to_owned(),
                (0, o) => format!("{o} encodes"),
                (_, 1) => "one encode shared, one of its own".to_owned(),
                (_, o) => format!("one encode shared, {o} of their own"),
            };
            format!("Upload needed: about {mbps} Mbit/s for {n} destinations ({how}).")
        }
    };
    (upload, warnings)
}

/// The inline editor of one destination: a copy, written on Save.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Editor {
    /// The destination being edited; `None` while adding one.
    pub id: Option<u32>,
    pub canvas: Canvas,
    pub draft: Destination,
    /// The draft's server field changed and differs from what was saved.
    pub problem: String,
}

impl Editor {
    /// Edits `r`, a resolved destination (its first one's server from the stream).
    pub fn edit(r: &Resolved, canvas: Canvas) -> Self {
        Self {
            id: Some(r.destination.id),
            canvas,
            draft: r.destination.clone(),
            problem: String::new(),
        }
    }

    /// Adds a destination to `canvas`, YouTube by default.
    pub fn add(canvas: Canvas) -> Self {
        let mut draft = Destination::new(u32::MAX, canvas, "", CUSTOM);
        let mut e = Self {
            id: None,
            canvas,
            draft: draft.clone(),
            problem: String::new(),
        };
        if let Some(first) = stream_services::all().first() {
            e.set_service(first);
        } else {
            draft.service = CUSTOM.to_owned();
        }
        e
    }

    /// The first destination of its canvas (its server and key are the stream's).
    pub fn first(&self) -> bool {
        self.draft.is_first()
    }

    /// The preset's index in [`stream_services::all`].
    pub fn service_index(&self) -> usize {
        let all = stream_services::all();
        let id = service_of(&self.draft).map_or(CUSTOM, |s| s.id.as_str());
        all.iter()
            .position(|s| s.id == id)
            .unwrap_or(all.len().saturating_sub(1))
    }

    /// Picks a preset: it fills the server (a per-account one empties it) and the
    /// name, when the name was empty or the previous preset's.
    pub fn set_service(&mut self, s: &Service) {
        let old = service_of(&self.draft).map(|o| o.name.clone());
        if self.draft.name.trim().is_empty() || old.as_deref() == Some(self.draft.name.as_str()) {
            self.draft.name = if s.id == CUSTOM {
                String::new()
            } else {
                s.name.clone()
            };
        }
        self.draft.service = s.id.clone();
        if let Some(server) = s.servers.first() {
            self.draft.server = server.url.clone();
        } else if s.server_from_dashboard || s.id == CUSTOM {
            // A server another preset filled is not this one's.
            if stream_services::by_server(&self.draft.server).is_some() {
                self.draft.server.clear();
            }
        }
        self.problem.clear();
    }

    /// The server typed. A server that is not valid stays in the field with the reason.
    pub fn set_server(&mut self, text: &str) {
        let url = text.trim();
        self.problem = match url {
            "" => String::new(),
            u => parse_server(u).err().unwrap_or_default(),
        };
        self.draft.server = url.to_owned();
    }

    /// "Same as the stream" (`false`) or "Own settings", starting from `stream`.
    pub fn set_own(&mut self, own: bool, stream: &Encoding) {
        self.draft.encoding = match (own, &self.draft.encoding) {
            (false, _) => DestinationEncoding::Shared,
            (true, DestinationEncoding::Own { .. }) => return,
            (true, DestinationEncoding::Shared) => DestinationEncoding::Own {
                encoding: stream.clone(),
                output_size: None,
            },
        };
    }

    /// The help under the key field: where the service shows it.
    pub fn key_help(&self) -> String {
        service_of(&self.draft)
            .map(|s| s.key_help.clone())
            .unwrap_or_default()
    }

    /// The help under the server field: a per-account server says where to find it.
    pub fn server_help(&self) -> String {
        match stream_services::get(&self.draft.service) {
            Some(s) if s.server_from_dashboard => s.key_help.clone(),
            _ => String::new(),
        }
    }

    /// Writes the draft into `all` (and, for a first destination, its server,
    /// reconnect and SRT latency into `stream`). Returns the destination's id, or why
    /// it cannot be saved.
    pub fn save(&self, all: &mut Destinations, stream: &mut StreamProfile) -> Result<u32, String> {
        if !self.problem.is_empty() {
            return Err(self.problem.clone());
        }
        let mut next = all.clone();
        let id = match self.id {
            Some(id) => id,
            None => next.add(self.canvas, &self.draft.name, &self.draft.service)?,
        };
        let entry = next
            .entry_mut(id)
            .ok_or_else(|| format!("there is no destination {id}"))?;
        let key_saved = entry.key_saved;
        *entry = Destination {
            id,
            key_saved,
            ..self.draft.clone()
        };
        if self.first() {
            // The stream section keeps these for the first destination.
            stream.server = self.draft.server.clone();
            stream.reconnect = self.draft.reconnect;
            stream.srt_latency_ms = self.draft.srt_latency_ms;
            entry.server.clear();
            entry.key_saved = false;
        }
        if let Some(why) = next.problem() {
            return Err(why);
        }
        *all = next;
        Ok(id)
    }
}

/// A new destination can be added to `canvas`.
pub(crate) fn can_add(all: &Destinations, canvas: Canvas) -> bool {
    all.entries
        .iter()
        .filter(|d| d.canvas == canvas && !d.is_first())
        .count()
        + 1
        < MAX_DESTINATIONS_PER_CANVAS
}

/// "3 of 6".
pub(crate) fn count(all: &Destinations, canvas: Canvas) -> String {
    let n = all
        .entries
        .iter()
        .filter(|d| d.canvas == canvas && !d.is_first())
        .count()
        + 1;
    format!("{n} of {MAX_DESTINATIONS_PER_CANVAS}")
}

/// `Studio` model rows of the card.
pub(crate) fn row_model(rows: &[Row]) -> slint::ModelRc<crate::ui::DestinationRow> {
    std::rc::Rc::new(slint::VecModel::from(
        rows.iter()
            .map(|r| crate::ui::DestinationRow {
                id: r.id as i32,
                name: r.name.as_str().into(),
                note: r.note.as_str().into(),
                on: r.on,
                warn: r.warn,
            })
            .collect::<Vec<_>>(),
    ))
    .into()
}

impl super::StudioUi {
    /// The picked canvas's destinations may change: its stream does not run, and
    /// the saved section is one this version may write.
    pub(crate) fn destinations_editable(&self) -> bool {
        self.stream_form_idle() && self.destinations_note.is_empty()
    }

    /// The Use on Go live switch of `id`.
    pub fn dest_set_enabled(&mut self, id: i32, on: bool) {
        let Ok(id) = u32::try_from(id) else { return };
        if self.destinations_editable() && self.destinations.set_enabled(id, on) {
            self.save_destinations();
        }
    }

    /// The edit button of `id`: opens the editor on a copy.
    pub fn dest_edit(&mut self, id: i32) {
        let Ok(id) = u32::try_from(id) else { return };
        if !self.destinations_editable() {
            return;
        }
        self.commit_server();
        let canvas = role(self.settings_canvas);
        let found = self
            .destinations
            .of(canvas, self.stream_form(), "")
            .into_iter()
            .find(|r| r.destination.id == id);
        if let Some(r) = found {
            self.dest_editor = Some(Editor::edit(&r, canvas));
            self.dest_serial += 1;
        }
    }

    /// "Add destination".
    pub fn dest_add(&mut self) {
        let canvas = role(self.settings_canvas);
        if self.destinations_editable() && can_add(&self.destinations, canvas) {
            self.dest_editor = Some(Editor::add(canvas));
            self.dest_serial += 1;
        }
    }

    /// The editor's Service.
    pub fn dest_set_service(&mut self, i: i32) {
        let Some(s) = usize::try_from(i)
            .ok()
            .and_then(|i| stream_services::all().get(i))
        else {
            return;
        };
        if let Some(e) = &mut self.dest_editor {
            e.set_service(s);
            self.dest_serial += 1;
        }
    }

    pub fn dest_set_name(&mut self, text: &str) {
        if let Some(e) = &mut self.dest_editor {
            e.draft.name = text.trim().to_owned();
        }
    }

    pub fn dest_set_server(&mut self, text: &str) {
        if let Some(e) = &mut self.dest_editor {
            e.set_server(text);
        }
    }

    pub fn dest_set_own(&mut self, own: bool) {
        let stream = self.stream_form().encoding.clone();
        if let Some(e) = &mut self.dest_editor {
            e.set_own(own, &stream);
        }
    }

    pub fn dest_set_reconnect(&mut self, on: bool) {
        if let Some(e) = &mut self.dest_editor {
            e.draft.reconnect = on;
        }
    }

    /// The editor's Save key (`None`: Remove key). A new destination is saved first,
    /// so its key has an id to be kept under.
    pub fn dest_save_key(&mut self, key: Option<&str>) {
        let key = match key.map(str::trim) {
            Some("") => return,
            Some(k) => Some(rbs_transport::Secret::new(k)),
            None => None,
        };
        let Some(e) = &self.dest_editor else { return };
        if e.first() {
            let is_main = self.settings_canvas == OutputCanvas::Main;
            self.send(match is_main {
                true => rbs_app::studio_host::Command::SaveStreamKey(key),
                false => rbs_app::studio_host::Command::SaveVerticalStreamKey(key),
            });
            return;
        }
        let id = match e.id {
            Some(id) => id,
            None => match self.dest_commit() {
                Some(id) => id,
                None => return,
            },
        };
        self.send(rbs_app::studio_host::Command::SaveDestinationKey(id, key));
    }

    /// Writes the draft; it stays open on the saved destination. `None` when it
    /// cannot be saved (the editor shows why).
    fn dest_commit(&mut self) -> Option<u32> {
        let mut e = self.dest_editor.take()?;
        let mut all = self.destinations.clone();
        let mut stream = self.stream_form().clone();
        let result = e.save(&mut all, &mut stream);
        let id = match result {
            Ok(id) => id,
            Err(why) => {
                e.problem = why;
                self.dest_editor = Some(e);
                return None;
            }
        };
        let first = e.first();
        e.id = Some(id);
        e.draft.id = id;
        self.dest_editor = Some(e);
        self.destinations = all;
        self.save_destinations();
        if first && stream != *self.stream_form() {
            *self.stream_form_mut() = stream;
            self.server_serial += 1;
            self.save_stream_form();
        }
        Some(id)
    }

    /// The editor's Save: writes the draft and closes it.
    pub fn dest_save(&mut self) {
        if self.dest_commit().is_some() {
            self.dest_editor = None;
        }
        self.dest_serial += 1;
    }

    pub fn dest_cancel(&mut self) {
        self.dest_editor = None;
        self.dest_serial += 1;
    }

    /// "Remove destination": removes it and its key. The first one cannot be removed.
    pub fn dest_remove(&mut self) {
        let Some(id) = self.dest_editor.as_ref().and_then(|e| e.id) else {
            self.dest_cancel();
            return;
        };
        match self.destinations.remove(id) {
            Ok(_) => {
                self.save_destinations();
                self.send(rbs_app::studio_host::Command::SaveDestinationKey(id, None));
                self.dest_cancel();
            }
            Err(why) => {
                if let Some(e) = &mut self.dest_editor {
                    e.problem = why;
                }
            }
        }
    }

    /// [`rbs_app::studio_host::Update::DestinationKey`].
    pub(crate) fn dest_key_saved(&mut self, id: u32, result: Result<bool, String>) {
        match result {
            Ok(saved) => {
                if let Some(d) = self.destinations.entry_mut(id) {
                    d.key_saved = saved;
                    self.save_destinations();
                }
                if let Some(e) = &mut self.dest_editor
                    && e.id == Some(id)
                {
                    e.draft.key_saved = saved;
                }
                self.key_serial += 1;
            }
            Err(why) => self.stream_note = why,
        }
    }

    /// The editor's draft encoding while "Own settings", for the encoder fields.
    pub(crate) fn dest_encoding(&self) -> Option<Encoding> {
        match &self.dest_editor.as_ref()?.draft.encoding {
            DestinationEncoding::Own { encoding, .. } => Some(encoding.clone()),
            DestinationEncoding::Shared => None,
        }
    }

    /// The encoder fields changed the draft's own encoding.
    pub(crate) fn dest_set_encoding(&mut self, e: Encoding) {
        if let Some(ed) = &mut self.dest_editor
            && let DestinationEncoding::Own { encoding, .. } = &mut ed.draft.encoding
        {
            *encoding = e;
        }
    }

    /// Fills the card and the editor.
    pub(crate) fn push_destinations(&self, g: &crate::ui::Studio<'_>) {
        let canvas = role(self.settings_canvas);
        let stream = self.stream_form();
        g.set_st_dests(row_model(&rows(&self.destinations, canvas, stream)));
        g.set_st_dest_count(count(&self.destinations, canvas).into());
        g.set_st_dest_can_add(self.destinations_editable() && can_add(&self.destinations, canvas));
        g.set_st_dest_editable(self.destinations_editable());
        let (upload, warnings) = notes(&self.destinations, canvas, stream);
        g.set_st_dest_upload(upload.into());
        g.set_st_dest_warnings(warnings.join("\n").into());
        let e = self.dest_editor.as_ref();
        g.set_st_ed_open(e.is_some());
        let Some(e) = e else { return };
        if g.get_st_ed_serial() != self.dest_serial as i32 {
            g.set_st_ed_name(e.draft.name.as_str().into());
            g.set_st_ed_server(e.draft.server.as_str().into());
            g.set_st_ed_serial(self.dest_serial as i32);
        }
        g.set_st_ed_new(e.id.is_none());
        g.set_st_ed_first(e.first());
        g.set_st_ed_services(super::strings(
            stream_services::all().iter().map(|s| s.name.clone()),
        ));
        g.set_st_ed_service(e.service_index() as i32);
        g.set_st_ed_server_help(e.server_help().into());
        g.set_st_ed_key_help(e.key_help().into());
        let key_saved = if e.first() {
            stream.key_saved
        } else {
            e.draft.key_saved
        };
        g.set_st_ed_key_saved(key_saved);
        g.set_st_ed_own(e.draft.encoding.is_own());
        g.set_st_ed_reconnect(e.draft.reconnect);
        g.set_st_ed_problem(e.problem.as_str().into());
        g.set_st_ed_title(
            if e.id.is_none() {
                "Add destination".to_owned()
            } else {
                format!("Edit {}", label(&e.draft))
            }
            .into(),
        );
        if e.draft.encoding.is_own() {
            g.set_st_ed_enc(self.encoder_data(super::output::DESTINATION));
        }
    }
}

/// One destination of the running stream as the live chip's list shows it
/// (ADR-0127 decision 17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveRow {
    pub id: u32,
    pub name: String,
    /// Its state in words.
    pub state: String,
    /// Time on air and drops, or why it failed.
    pub detail: String,
    /// It failed or reconnects: shown in the warning colour, never by colour alone.
    pub problem: bool,
    pub can_stop: bool,
    pub can_retry: bool,
}

/// "0:01:05", as the live time.
fn clock(secs: u64) -> String {
    format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// The list rows of the destinations `rows` (each with when it arrived), as of
/// `now`.
pub(crate) fn live_rows(rows: &[(LiveDestination, Instant)], now: Instant) -> Vec<LiveRow> {
    rows.iter()
        .map(|(d, at)| {
            let running = matches!(
                d.state,
                DestinationState::Connecting
                    | DestinationState::Live
                    | DestinationState::Reconnecting
            );
            let on_air = if d.on_air_ms > 0 || d.state == DestinationState::Live {
                d.on_air_ms / 1000 + now.saturating_duration_since(*at).as_secs()
            } else {
                0
            };
            let drops = match d.dropped {
                0 => String::new(),
                n => format!(" · {n} dropped"),
            };
            let (state, detail) = match d.state {
                DestinationState::Connecting => ("Connecting".to_owned(), String::new()),
                DestinationState::Live => ("Live".to_owned(), format!("{}{drops}", clock(on_air))),
                DestinationState::Reconnecting => (
                    format!("Reconnecting, try {}", d.reconnect_attempts.max(1)),
                    format!("{}{drops}", clock(on_air)),
                ),
                DestinationState::Failed => {
                    ("Failed".to_owned(), d.error.clone().unwrap_or_default())
                }
                DestinationState::Stopped => ("Stopped".to_owned(), String::new()),
            };
            LiveRow {
                id: d.id,
                name: d.name.clone(),
                state,
                detail,
                problem: matches!(
                    d.state,
                    DestinationState::Failed | DestinationState::Reconnecting
                ),
                can_stop: running,
                can_retry: !running,
            }
        })
        .collect()
}

/// The chip's count, "2/3" (destinations live of those Go live started); empty with
/// one destination, where the chip says LIVE alone.
pub(crate) fn live_count(rows: &[(LiveDestination, Instant)]) -> String {
    if rows.len() < 2 {
        return String::new();
    }
    let live = rows
        .iter()
        .filter(|(d, _)| d.state == DestinationState::Live)
        .count();
    format!("{live}/{}", rows.len())
}

/// `Shell` model rows of the live chip's list.
pub(crate) fn live_model(rows: &[LiveRow]) -> slint::ModelRc<crate::ui::LiveDestinationRow> {
    std::rc::Rc::new(slint::VecModel::from(
        rows.iter()
            .map(|r| crate::ui::LiveDestinationRow {
                id: r.id as i32,
                name: r.name.as_str().into(),
                state: r.state.as_str().into(),
                detail: r.detail.as_str().into(),
                problem: r.problem,
                can_stop: r.can_stop,
                can_retry: r.can_retry,
            })
            .collect::<Vec<_>>(),
    ))
    .into()
}

/// The app log lines for what changed between two lists of running destinations: each
/// one that started, changed state (with its error, never a URL or key), or left.
fn destination_log_lines<'a>(
    old: impl Iterator<Item = &'a LiveDestination>,
    new: &[LiveDestination],
) -> Vec<String> {
    let old: Vec<&LiveDestination> = old.collect();
    let label = |d: &LiveDestination| {
        if d.name.is_empty() {
            format!("destination {}", d.id)
        } else {
            format!("destination {} ({})", d.id, d.name)
        }
    };
    let state = |d: &LiveDestination| format!("{:?}", d.state).to_lowercase();
    let mut lines = Vec::new();
    for d in new {
        match old.iter().find(|o| o.id == d.id) {
            None => lines.push(format!("{} started: {}", label(d), state(d))),
            Some(o) if o.state != d.state => {
                let mut line = format!(
                    "{}: {} (connected {} times, {} reconnect attempts)",
                    label(d),
                    state(d),
                    d.connects,
                    d.reconnect_attempts
                );
                if let Some(why) = &d.error {
                    line.push_str(&format!(", {why}"));
                }
                lines.push(line);
            }
            Some(_) => {}
        }
    }
    for o in &old {
        if !new.iter().any(|d| d.id == o.id) {
            lines.push(format!("{} stopped", label(o)));
        }
    }
    lines
}

impl super::StudioUi {
    /// The main canvas's running destinations changed (`Update::LiveDestinations`).
    pub(crate) fn live_destinations_changed(
        &mut self,
        c: OutputCanvas,
        rows: Vec<LiveDestination>,
    ) {
        if c != OutputCanvas::Main {
            return;
        }
        for line in destination_log_lines(self.live_dests.iter().map(|(d, _)| d), &rows) {
            super::log(&line);
        }
        let now = Instant::now();
        self.live_dests = rows.into_iter().map(|d| (d, now)).collect();
    }

    /// The live chip's list as of now.
    pub(crate) fn live_destination_rows(&self) -> Vec<LiveRow> {
        live_rows(&self.live_dests, Instant::now())
    }

    /// The live chip's count ("2/3"), empty with one destination.
    pub(crate) fn live_destination_count(&self) -> String {
        live_count(&self.live_dests)
    }

    /// Stop from the live chip's list.
    pub(crate) fn live_dest_stop(&mut self, id: u32) {
        self.send(rbs_app::studio_host::Command::StopDestination(id));
    }

    /// Retry from the live chip's list.
    pub(crate) fn live_dest_retry(&mut self, id: u32) {
        self.send(rbs_app::studio_host::Command::StartDestination(id));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn row_in(id: u32, name: &str, state: DestinationState) -> LiveDestination {
        LiveDestination {
            id,
            name: name.into(),
            service: "custom".into(),
            state,
            connects: 1,
            reconnect_attempts: 0,
            dropped: 0,
            on_air_ms: 0,
            error: None,
        }
    }

    #[test]
    fn the_log_names_every_destination_that_starts_changes_or_leaves() {
        let old = [
            row_in(0, "Twitch", DestinationState::Live),
            row_in(2, "", DestinationState::Live),
        ];
        let mut failed = row_in(2, "", DestinationState::Failed);
        failed.error = Some("refused".into());
        let new = [
            row_in(0, "Twitch", DestinationState::Live),
            failed,
            row_in(3, "Kick", DestinationState::Connecting),
        ];
        let lines = destination_log_lines(old.iter(), &new);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("destination 2: failed"), "{lines:?}");
        assert!(lines[0].ends_with("refused"), "{lines:?}");
        assert_eq!(lines[1], "destination 3 (Kick) started: connecting");
        let ended = destination_log_lines(new.iter(), &[]);
        assert_eq!(ended.len(), 3);
        assert_eq!(ended[2], "destination 3 (Kick) stopped");
    }

    fn stream(server: &str) -> StreamProfile {
        let mut s = StreamProfile::new();
        s.server = server.into();
        s.key_saved = true;
        s
    }

    #[test]
    fn rows_show_the_first_destination_then_the_others_with_what_is_missing() {
        let mut all = Destinations::default();
        let id = all.add(Canvas::Main, "", "kick").unwrap();
        let s = stream("rtmp://live.twitch.tv/app");
        let rows = rows(&all, Canvas::Main, &s);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "Twitch");
        assert_eq!(
            rows[0].note,
            "Twitch · RTMP · Key saved · Same as the stream"
        );
        assert!(!rows[0].warn);
        assert_eq!(rows[1].id, id);
        assert_eq!(rows[1].name, "Kick");
        assert_eq!(rows[1].note, "Kick · No server yet · Same as the stream");
        assert!(rows[1].warn);
        assert_eq!(count(&all, Canvas::Main), "2 of 6");
    }

    #[test]
    fn a_preset_fills_the_server_and_name_and_the_draft_saves() {
        let mut all = Destinations::default();
        let mut s = stream("rtmp://live.twitch.tv/app");
        let mut e = Editor::add(Canvas::Main);
        assert_eq!(e.draft.name, "YouTube");
        assert!(e.draft.server.starts_with("rtmps://"));
        e.set_service(stream_services::get("kick").unwrap());
        assert_eq!(e.draft.name, "Kick");
        assert_eq!(e.draft.server, "");
        assert!(!e.server_help().is_empty());
        e.set_server("rtmp://");
        assert!(!e.problem.is_empty());
        assert!(e.save(&mut all, &mut s).is_err());
        e.set_server("rtmps://fa723fc1b171.global-contribute.live-video.net/app");
        e.set_own(true, &s.encoding);
        let id = e.save(&mut all, &mut s).unwrap();
        let d = all.get(id).unwrap();
        assert_eq!(d.name, "Kick");
        assert!(d.encoding.is_own());
        // Editing the first destination writes its server into the stream section.
        let first = all.of(Canvas::Main, &s, "").into_iter().next().unwrap();
        let mut e = Editor::edit(&first, Canvas::Main);
        e.set_server("rtmp://a.rtmp.youtube.com/live2");
        e.draft.name = "Main".into();
        e.save(&mut all, &mut s).unwrap();
        assert_eq!(s.server, "rtmp://a.rtmp.youtube.com/live2");
        assert_eq!(all.get(0).map(|d| d.name.as_str()), Some("Main"));
        assert_eq!(all.get(0).map(|d| d.server.as_str()), Some(""));
    }

    #[test]
    fn notes_sum_the_upload_and_warn_about_service_limits() {
        let mut all = Destinations::default();
        let mut s = stream("rtmp://live.twitch.tv/app");
        s.encoding.video_kbps = 8000;
        assert_eq!(
            notes(&all, Canvas::Main, &s),
            (
                String::new(),
                vec!["Twitch: 8000 kbit/s is above the 6000 kbit/s Twitch accepts.".to_owned()]
            )
        );
        let mut e = Editor::add(Canvas::Main);
        e.set_own(true, &s.encoding);
        e.save(&mut all, &mut s).unwrap();
        let (upload, _) = notes(&all, Canvas::Main, &s);
        assert_eq!(
            upload,
            "Upload needed: about 25 Mbit/s for 2 destinations (one encode shared, one of its own)."
        );
    }

    fn live(id: u32, state: DestinationState, on_air_ms: u64, dropped: u64) -> LiveDestination {
        LiveDestination {
            id,
            name: format!("D{id}"),
            service: "custom".to_owned(),
            state,
            connects: 1,
            reconnect_attempts: 2,
            dropped,
            on_air_ms,
            error: (state == DestinationState::Failed).then(|| "refused the key".to_owned()),
        }
    }

    #[test]
    fn the_live_list_says_each_state_in_words_and_counts_those_live() {
        let at = Instant::now();
        let later = at + std::time::Duration::from_secs(5);
        let rows = vec![
            (live(0, DestinationState::Live, 60_000, 0), at),
            (live(1, DestinationState::Reconnecting, 30_000, 12), at),
            (live(2, DestinationState::Failed, 0, 0), at),
            (live(3, DestinationState::Stopped, 0, 0), at),
        ];
        let out = live_rows(&rows, later);
        assert_eq!(out[0].state, "Live");
        assert_eq!(out[0].detail, "0:01:05");
        assert!(!out[0].problem && out[0].can_stop && !out[0].can_retry);
        assert_eq!(out[1].state, "Reconnecting, try 2");
        assert_eq!(out[1].detail, "0:00:35 · 12 dropped");
        assert!(out[1].problem && out[1].can_stop);
        assert_eq!(out[2].detail, "refused the key");
        assert!(out[2].problem && out[2].can_retry && !out[2].can_stop);
        assert!(!out[3].problem && out[3].can_retry);
        assert_eq!(live_count(&rows), "1/4");
        assert_eq!(live_count(&rows[..1]), "");
    }
}
