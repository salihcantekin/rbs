//! The guest window (ADR-0139 phase 1d, docs/specs/peer-port.md): what the Share and
//! Add guest flows show, as pure functions of the state the control API reports
//! (`guest.status`), so the pages are tested without a window.
//!
//! - [`Flow`]: this side sends its program (Share) or receives a guest (Add guest).
//! - [`Status`]: one share's or guest's state as `guest.status` reports it.
//! - [`view`]: the page, texts, step marks and button states for a flow and a status.
//! - [`Session`]: the window's state between two answers. It never calls the API: it
//!   queues [`Call`]s (`take_calls`) and takes the answers (`answer`), so the page
//!   sends them as control-API commands without waiting and the logic is tested
//!   with scripted answers.

/// Which flow the window runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// This RBS sends its program: invite code out, answer code in.
    Share,
    /// This RBS receives a guest: invite code in, answer code out.
    Host,
}

/// A pairing's state, as the control API names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Collecting addresses; the code is not ready.
    Gathering,
    /// A share's invite waits for the answer.
    WaitingForAnswer,
    /// The answer is made or taken; the connection is being set up.
    Connecting,
    /// Connected; this side has not confirmed the phrase.
    CheckPhrase,
    /// Confirmed and running.
    Live,
    /// Ended with a reason.
    Failed,
    /// Closed by a user.
    Closed,
}

impl State {
    /// Reads the API's `state` string; unknown values read as `Closed`.
    pub fn parse(s: &str) -> Self {
        match s {
            "gathering" => Self::Gathering,
            "waiting_for_answer" => Self::WaitingForAnswer,
            "connecting" => Self::Connecting,
            "check_phrase" => Self::CheckPhrase,
            "live" => Self::Live,
            "failed" => Self::Failed,
            _ => Self::Closed,
        }
    }
}

/// One share's or guest's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The state.
    pub state: State,
    /// The six check words once connected.
    pub phrase: Vec<String>,
    /// The `guest_*` reason of a failure.
    pub reason: Option<String>,
    /// Seconds until the pending code expires.
    pub expires_in_s: Option<u32>,
}

impl Status {
    /// The state before the first answer of the API.
    pub fn gathering() -> Self {
        Self {
            state: State::Gathering,
            phrase: Vec::new(),
            reason: None,
            expires_in_s: None,
        }
    }
}

/// The page the window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// Share: the invite and the answer box. Host: the invite box.
    Invite,
    /// Host: the answer code to send back.
    Answer,
    /// The six words and the two answers.
    Compare,
    /// Running: a status line and Stop.
    Live,
    /// A failure and what to do.
    Failed,
}

impl Page {
    /// The number the window's `page` property uses.
    pub fn index(self) -> i32 {
        match self {
            Self::Invite => 0,
            Self::Answer => 1,
            Self::Compare => 2,
            Self::Live => 3,
            Self::Failed => 4,
        }
    }
}

/// A step of the header strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Not reached.
    Todo,
    /// The current step.
    Now,
    /// Finished.
    Done,
}

impl Mark {
    /// The number the window's `steps` use: 0 not reached, 1 current, 2 done.
    pub fn index(self) -> i32 {
        match self {
            Self::Todo => 0,
            Self::Now => 1,
            Self::Done => 2,
        }
    }
}

/// Tone of a status badge: 0 neutral, 1 healthy, 2 error, 3 warning.
pub type Tone = i32;

/// Everything the window draws for one status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// The page.
    pub page: Page,
    /// The window title.
    pub title: String,
    /// The line under the title.
    pub hint: String,
    /// The marks of Invite, Answer and Compare.
    pub steps: [Mark; 3],
    /// The badge text; empty for none.
    pub badge: String,
    /// The badge's tone.
    pub tone: Tone,
    /// Time left on a pending code, "7:42"; empty for none.
    pub expires: String,
    /// The text of the failure page.
    pub failure: String,
    /// The paste box (Share's answer, Host's invite) takes input.
    pub paste_enabled: bool,
    /// The code box shows a code.
    pub code_ready: bool,
}

/// "7:42" for 462 seconds.
pub fn clock(seconds: u32) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The plain text of a `guest_*` reason (ADR-0139 decisions 7, 10, 12 and 13).
pub fn reason_text(reason: &str) -> &'static str {
    match reason {
        "guest_expired" => "The code ran out of time. Start again to get new codes.",
        "guest_no_public_address" => {
            "No public address was found. Check the network, or share on the same local network."
        }
        "guest_unreachable" => {
            "The two computers could not reach each other. A strict network can block this; try another network."
        }
        "guest_disconnected" => "The connection was lost. Start again to get new codes.",
        "guest_return_feed" => {
            "Sharing a program that contains a guest needs a return mix, which comes later."
        }
        "guest_limits" => "No more guests or shares can run now. Stop one first.",
        "guest_code_wrong_session" => "That code belongs to another invite.",
        "guest_not_a_code" => "That is not a guest code. Paste the whole code.",
        "guest_code_damaged" => "The code is damaged. Copy it again, all of it.",
        "guest_code_newer" => "This code is from a newer RBS. Update this RBS.",
        "guest_code_transport" => "This code uses a connection type this RBS does not have.",
        "guest_code_wrong_kind" => {
            "That is an invite where an answer is needed, or the other way round."
        }
        "guest_output_failed" => "The share could not start sending. Start again.",
        "no_encoder_slot" | "too_many_outputs" => {
            "There is no free encoder for a share now. Stop an output first."
        }
        _ => "The pairing failed. Start again to get new codes.",
    }
}

/// The page for `flow` and `status`.
pub fn view(flow: Flow, status: &Status) -> View {
    let (page, steps) = match status.state {
        State::Gathering | State::WaitingForAnswer => {
            (Page::Invite, [Mark::Now, Mark::Todo, Mark::Todo])
        }
        State::Connecting if flow == Flow::Host => {
            (Page::Answer, [Mark::Done, Mark::Now, Mark::Todo])
        }
        State::Connecting => (Page::Invite, [Mark::Done, Mark::Now, Mark::Todo]),
        State::CheckPhrase => (Page::Compare, [Mark::Done, Mark::Done, Mark::Now]),
        State::Live => (Page::Live, [Mark::Done, Mark::Done, Mark::Done]),
        State::Failed | State::Closed => (Page::Failed, [Mark::Todo, Mark::Todo, Mark::Todo]),
    };
    let (title, hint) = match (flow, page) {
        (Flow::Share, Page::Invite) => (
            "Share program",
            "Send your program to another RBS over a direct connection.",
        ),
        (Flow::Host, Page::Invite) => ("Guest", "Paste the invite code your guest sent you."),
        (Flow::Host, Page::Answer) => ("Guest", "Send this answer code to your guest."),
        (Flow::Share, Page::Compare) => (
            "Compare the check phrase",
            "Connected. Nothing is shared yet.",
        ),
        (Flow::Host, Page::Compare) => (
            "Compare the check phrase",
            "Connected. The guest stays hidden until you confirm.",
        ),
        (Flow::Share, Page::Live) => ("Sharing", "Your program is going to the other RBS."),
        (Flow::Host, Page::Live) => ("Guest", "The guest is connected."),
        (Flow::Share, Page::Answer) => ("Share program", ""),
        (_, Page::Failed) => (
            if flow == Flow::Share {
                "Share program"
            } else {
                "Guest"
            },
            "",
        ),
    };
    let (badge, tone): (&str, Tone) = match status.state {
        State::Gathering => ("Preparing…", 0),
        State::WaitingForAnswer => ("Waiting for answer", 3),
        State::Connecting => ("Connecting…", 3),
        State::CheckPhrase => ("Not verified", 3),
        State::Live => ("Connected", 1),
        State::Failed => ("Failed", 2),
        State::Closed => ("Closed", 0),
    };
    let failure = match (&status.reason, status.state) {
        (Some(r), State::Failed) => reason_text(r).to_string(),
        (None, State::Failed) => reason_text("").to_string(),
        _ => String::new(),
    };
    View {
        page,
        title: title.into(),
        hint: hint.into(),
        steps,
        badge: badge.into(),
        tone,
        expires: match (status.state, status.expires_in_s) {
            (State::WaitingForAnswer | State::Connecting, Some(s)) => clock(s),
            _ => String::new(),
        },
        failure,
        paste_enabled: match flow {
            Flow::Share => status.state == State::WaitingForAnswer,
            Flow::Host => false,
        },
        code_ready: !matches!(
            status.state,
            State::Gathering | State::Failed | State::Closed
        ),
    }
}

/// An error of a call: a typed `guest_*` reason, or a short text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallError(pub String);

/// One control-API call the window makes (protocol 1.116, `control-api.md`).
#[derive(Clone, PartialEq, Eq)]
pub enum Call {
    /// `guest.share`.
    Share,
    /// `guest.share.code` for a share.
    Code(u32),
    /// `guest.share.accept_answer` for a share; the text is a secret.
    Answer(u32, String),
    /// `guest.share.confirm`.
    Confirm(u32),
    /// `guest.share.stop`.
    Stop(u32),
    /// `guest.status`.
    Status,
    /// Adding a guest source from an invite (the host side of ADR-0139; the `guest.source.*`
    /// methods come with the guest source). The text is a secret.
    AddGuest(String),
}

impl std::fmt::Debug for Call {
    /// Never prints a code.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Answer(id, _) => write!(f, "Answer({id}, <secret>)"),
            Self::AddGuest(_) => write!(f, "AddGuest(<secret>)"),
            Self::Share => write!(f, "Share"),
            Self::Code(id) => write!(f, "Code({id})"),
            Self::Confirm(id) => write!(f, "Confirm({id})"),
            Self::Stop(id) => write!(f, "Stop({id})"),
            Self::Status => write!(f, "Status"),
        }
    }
}

/// One share of a `guest.status` answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The share's id.
    pub share: u32,
    /// Its state.
    pub state: State,
    /// The six check words, once connected.
    pub phrase: Vec<String>,
    /// The `guest_*` reason of a failure.
    pub reason: Option<String>,
    /// When the invite expires, seconds since the Unix epoch.
    pub expires_at: Option<u64>,
}

/// An answer to a [`Call`].
#[derive(Clone, PartialEq, Eq)]
pub enum Reply {
    /// `guest.share`: the new share and its state.
    Started(u32, State),
    /// `guest.share.code`: the code, a secret.
    Code(String),
    /// `guest.share.accept_answer` and `guest.share.confirm`: the state after.
    State(State),
    /// `guest.status`: the shares, oldest first.
    Rows(Vec<Row>),
    /// A call with no data back (`guest.share.stop`).
    Done,
}

/// Splits the status's phrase into its words.
pub fn words(phrase: &str) -> Vec<String> {
    phrase
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether an error text is a typed `guest_*` reason (or an encoder-slot refusal)
/// rather than a plain message.
fn is_reason(e: &str) -> bool {
    e.starts_with("guest_") || e == "no_encoder_slot" || e == "too_many_outputs"
}

/// What the window holds between two answers: the pairing's id, its last status,
/// the code shown and the problem line. The window pushes [`Session::view`].
pub struct Session {
    flow: Flow,
    id: Option<u32>,
    status: Status,
    code: String,
    problem: String,
    calls: Vec<Call>,
    asked_share: bool,
    status_asked: bool,
    code_asked: bool,
}

impl Session {
    /// A session that has asked for nothing yet.
    pub fn new(flow: Flow) -> Self {
        Self {
            flow,
            id: None,
            status: Status::gathering(),
            code: String::new(),
            problem: String::new(),
            calls: Vec::new(),
            asked_share: false,
            status_asked: false,
            code_asked: false,
        }
    }

    /// The flow.
    pub fn flow(&self) -> Flow {
        self.flow
    }

    /// The pairing's id once it exists.
    pub fn id(&self) -> Option<u32> {
        self.id
    }

    /// The code to copy; empty until it is ready.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The problem line; empty when none.
    pub fn problem(&self) -> &str {
        &self.problem
    }

    /// The status last read.
    pub fn status(&self) -> &Status {
        &self.status
    }

    /// What the window shows now.
    pub fn view(&self) -> View {
        view(self.flow, &self.status)
    }

    /// The calls to send now, in order; the queue is emptied.
    pub fn take_calls(&mut self) -> Vec<Call> {
        std::mem::take(&mut self.calls)
    }

    /// Share asks for a share once; Add guest has nothing to ask until an invite is
    /// pasted.
    pub fn start(&mut self) {
        self.problem.clear();
        if self.flow == Flow::Share && !self.asked_share {
            self.asked_share = true;
            self.calls.push(Call::Share);
        }
    }

    /// The pasted text: Share's answer code, Add guest's invite code.
    pub fn submit(&mut self, text: &str) {
        self.problem.clear();
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        match (self.flow, self.id) {
            (Flow::Share, Some(id)) => self.calls.push(Call::Answer(id, text.to_string())),
            (Flow::Host, None) => self.calls.push(Call::AddGuest(text.to_string())),
            _ => {}
        }
    }

    /// Asks for the state (and the code once there is one to show), when no earlier
    /// ask is still out.
    pub fn tick(&mut self) {
        let Some(id) = self.id else { return };
        if matches!(self.status.state, State::Failed | State::Closed) {
            return;
        }
        if !self.status_asked {
            self.status_asked = true;
            self.calls.push(Call::Status);
        }
        let has_code = self.flow == Flow::Share && self.status.state == State::WaitingForAnswer;
        if has_code && self.code.is_empty() && !self.code_asked {
            self.code_asked = true;
            self.calls.push(Call::Code(id));
        }
    }

    /// The words match.
    pub fn confirm(&mut self) {
        self.problem.clear();
        if let Some(id) = self.id {
            self.calls.push(Call::Confirm(id));
        }
    }

    /// The words differ, or the user stops a running pairing: ends it.
    pub fn stop(&mut self) {
        self.problem.clear();
        if let Some(id) = self.id.take() {
            self.calls.push(Call::Stop(id));
        }
        self.code.clear();
        self.status = Status {
            state: State::Closed,
            ..Status::gathering()
        };
    }

    /// Start again after a failure or a stop: a fresh session of the same flow.
    pub fn again(&mut self) {
        let calls = std::mem::take(&mut self.calls);
        *self = Self::new(self.flow);
        self.calls = calls;
        self.start();
    }

    fn fail(&mut self, e: CallError) {
        if is_reason(&e.0) {
            self.status.state = State::Failed;
            self.status.reason = Some(e.0);
            self.problem.clear();
        } else {
            self.problem = e.0;
        }
    }

    /// Takes the answer to `call`. `now` is the time in seconds since the Unix epoch,
    /// to turn an expiry into time left.
    pub fn answer(&mut self, call: &Call, result: Result<Reply, CallError>, now: u64) {
        match call {
            Call::Status => self.status_asked = false,
            Call::Code(_) => self.code_asked = false,
            _ => {}
        }
        let reply = match result {
            Ok(r) => r,
            // A bad answer or invite keeps the pairing as it was.
            Err(CallError(e)) if matches!(call, Call::Answer(..) | Call::AddGuest(_)) => {
                self.problem = if is_reason(&e) {
                    reason_text(&e).into()
                } else {
                    e
                };
                return;
            }
            // A stop that fails leaves nothing to show beyond the text.
            Err(e) if matches!(call, Call::Stop(_)) => {
                self.problem = e.0;
                return;
            }
            Err(e) => return self.fail(e),
        };
        match reply {
            Reply::Started(id, state) => {
                self.id = Some(id);
                self.status.state = state;
            }
            Reply::Code(c) => self.code = c,
            Reply::State(state) => self.status.state = state,
            Reply::Rows(rows) => {
                let Some(id) = self.id else { return };
                match rows.into_iter().find(|r| r.share == id) {
                    Some(r) => {
                        self.status = Status {
                            state: r.state,
                            phrase: r.phrase,
                            reason: r.reason,
                            expires_in_s: r
                                .expires_at
                                .map(|e| u32::try_from(e.saturating_sub(now)).unwrap_or(u32::MAX)),
                        };
                        if matches!(r.state, State::CheckPhrase | State::Live) {
                            self.code.clear();
                        }
                    }
                    // The studio restarted: its shares are gone.
                    None => self.status.state = State::Closed,
                }
            }
            Reply::Done => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(state: State) -> Status {
        Status {
            state,
            phrase: Vec::new(),
            reason: None,
            expires_in_s: Some(462),
        }
    }

    fn row(state: State) -> Row {
        Row {
            share: 7,
            state,
            phrase: words("amber velvet harbor quartz meadow lantern"),
            reason: None,
            expires_at: Some(1_000 + 462),
        }
    }

    /// Plays the studio: answers each queued call as a scripted studio would.
    fn serve(s: &mut Session, state: State) {
        for call in s.take_calls() {
            let reply = match &call {
                Call::Share => Reply::Started(7, State::Gathering),
                Call::Code(_) => Reply::Code("rbs-guest:1:abc".into()),
                Call::Status => Reply::Rows(vec![row(state)]),
                Call::Answer(..) | Call::Confirm(_) => Reply::State(state),
                Call::AddGuest(_) => Reply::Started(3, State::Connecting),
                Call::Stop(_) => Reply::Done,
            };
            s.answer(&call, Ok(reply), 1_000);
        }
    }

    #[test]
    fn view_of_a_waiting_share_has_a_paste_box_and_a_clock() {
        let v = view(Flow::Share, &st(State::WaitingForAnswer));
        assert_eq!(v.page, Page::Invite);
        assert!(v.paste_enabled && v.code_ready);
        assert_eq!(v.expires, "7:42");
        assert_eq!(v.badge, "Waiting for answer");
        assert_eq!(v.steps, [Mark::Now, Mark::Todo, Mark::Todo]);
    }

    #[test]
    fn gathering_has_no_code_yet() {
        let v = view(Flow::Share, &Status::gathering());
        assert!(!v.code_ready && !v.paste_enabled);
        assert_eq!(v.expires, "");
    }

    #[test]
    fn host_shows_the_answer_code_while_connecting() {
        let v = view(Flow::Host, &st(State::Connecting));
        assert_eq!(v.page, Page::Answer);
        assert_eq!(v.steps, [Mark::Done, Mark::Now, Mark::Todo]);
        assert_eq!(view(Flow::Share, &st(State::Connecting)).page, Page::Invite);
    }

    #[test]
    fn both_sides_compare_then_run() {
        for flow in [Flow::Share, Flow::Host] {
            let c = view(flow, &st(State::CheckPhrase));
            assert_eq!(c.page, Page::Compare);
            assert_eq!(c.badge, "Not verified");
            assert_eq!(c.tone, 3);
            let l = view(flow, &st(State::Live));
            assert_eq!((l.page, l.tone), (Page::Live, 1));
            assert_eq!(l.expires, "");
        }
    }

    #[test]
    fn a_failure_names_its_reason_and_is_never_live_red() {
        let mut s = st(State::Failed);
        s.reason = Some("guest_return_feed".into());
        let v = view(Flow::Share, &s);
        assert_eq!(v.page, Page::Failed);
        assert!(
            v.failure
                .starts_with("Sharing a program that contains a guest")
        );
        assert_eq!(v.tone, 2);
        assert!(!view(Flow::Host, &st(State::Failed)).failure.is_empty());
    }

    #[test]
    fn states_parse_and_unknown_means_closed() {
        assert_eq!(State::parse("check_phrase"), State::CheckPhrase);
        assert_eq!(State::parse("waiting_for_answer"), State::WaitingForAnswer);
        assert_eq!(State::parse("what"), State::Closed);
    }

    #[test]
    fn clock_pads_seconds() {
        assert_eq!(clock(125), "2:05");
        assert_eq!(clock(0), "0:00");
    }

    #[test]
    fn the_phrase_splits_on_any_separator() {
        assert_eq!(
            words("amber-velvet, harbor  quartz meadow lantern").len(),
            6
        );
    }

    #[test]
    fn a_share_asks_once_then_fetches_its_code_when_it_waits() {
        let mut s = Session::new(Flow::Share);
        s.start();
        s.start();
        assert_eq!(s.take_calls(), vec![Call::Share]);
        s.answer(&Call::Share, Ok(Reply::Started(7, State::Gathering)), 1_000);
        s.tick();
        assert_eq!(s.take_calls(), vec![Call::Status]);
        s.tick();
        assert!(s.take_calls().is_empty(), "a status ask is still out");
        s.answer(
            &Call::Status,
            Ok(Reply::Rows(vec![row(State::WaitingForAnswer)])),
            1_000,
        );
        s.tick();
        assert_eq!(s.take_calls(), vec![Call::Status, Call::Code(7)]);
        s.answer(
            &Call::Code(7),
            Ok(Reply::Code("rbs-guest:1:abc".into())),
            1_000,
        );
        assert_eq!(s.code(), "rbs-guest:1:abc");
        assert_eq!(s.view().expires, "7:42");
        s.submit("  rbs-guest:1:answer \n");
        assert_eq!(
            s.take_calls(),
            vec![Call::Answer(7, "rbs-guest:1:answer".into())]
        );
    }

    #[test]
    fn a_bad_answer_keeps_the_share_waiting_with_a_problem_line() {
        let mut s = Session::new(Flow::Share);
        s.start();
        serve(&mut s, State::WaitingForAnswer);
        s.tick();
        serve(&mut s, State::WaitingForAnswer);
        s.answer(
            &Call::Answer(7, "x".into()),
            Err(CallError("guest_code_wrong_session".into())),
            1_000,
        );
        assert_eq!(s.view().page, Page::Invite);
        assert_eq!(s.problem(), "That code belongs to another invite.");
    }

    #[test]
    fn adding_a_guest_needs_an_invite_first() {
        let mut s = Session::new(Flow::Host);
        s.start();
        s.tick();
        assert!(s.take_calls().is_empty() && s.id().is_none());
        s.submit("rbs-guest:1:invite");
        serve(&mut s, State::Connecting);
        assert_eq!(s.id(), Some(3));
    }

    #[test]
    fn confirming_runs_and_the_code_goes_away() {
        let mut s = Session::new(Flow::Share);
        s.start();
        serve(&mut s, State::CheckPhrase);
        s.tick();
        serve(&mut s, State::CheckPhrase);
        assert_eq!(s.view().page, Page::Compare);
        assert_eq!(s.status().phrase.len(), 6);
        s.confirm();
        assert_eq!(s.take_calls(), vec![Call::Confirm(7)]);
        s.answer(&Call::Confirm(7), Ok(Reply::State(State::Live)), 1_000);
        assert_eq!(s.view().page, Page::Live);
        assert_eq!(s.code(), "");
    }

    #[test]
    fn stop_ends_the_pairing_and_again_starts_a_new_one() {
        let mut s = Session::new(Flow::Share);
        s.start();
        serve(&mut s, State::Live);
        s.stop();
        assert_eq!(s.take_calls(), vec![Call::Stop(7)]);
        assert_eq!(s.view().page, Page::Failed);
        s.tick();
        assert!(s.take_calls().is_empty());
        s.again();
        assert_eq!(s.take_calls(), vec![Call::Share]);
    }

    #[test]
    fn a_refused_share_fails_with_its_reason_and_a_plain_text_shows_a_problem() {
        let mut s = Session::new(Flow::Share);
        s.start();
        s.answer(&Call::Share, Err(CallError("guest_return_feed".into())), 0);
        assert_eq!(s.view().page, Page::Failed);
        assert!(s.view().failure.contains("return mix"));
        let mut s = Session::new(Flow::Share);
        s.start();
        s.answer(
            &Call::Share,
            Err(CallError("Guests are not here.".into())),
            0,
        );
        assert_eq!(s.view().page, Page::Invite);
        assert_eq!(s.problem(), "Guests are not here.");
    }

    #[test]
    fn a_share_missing_from_the_status_is_closed() {
        let mut s = Session::new(Flow::Share);
        s.start();
        serve(&mut s, State::Live);
        s.answer(&Call::Status, Ok(Reply::Rows(Vec::new())), 0);
        assert_eq!(s.status().state, State::Closed);
    }

    #[test]
    fn calls_never_print_a_code() {
        let c = Call::Answer(7, "rbs-guest:1:hunter2xyz".into());
        assert!(!format!("{c:?}").contains("hunter2xyz"));
        assert!(
            !format!("{:?}", Call::AddGuest("rbs-guest:1:hunter2xyz".into()))
                .contains("hunter2xyz")
        );
    }
}
