//! The guest window (ADR-0139 phase 1d, docs/specs/peer-port.md): what the Share and
//! Add guest flows show, as pure functions of the state the control API reports
//! (`guest.status`), so the pages are tested without a window.
//!
//! - [`Flow`]: this side sends its program (Share) or receives a guest (Add guest).
//! - [`Status`]: one share's or guest's state as `guest.status` reports it.
//! - [`view`]: the page, texts, step marks and button states for a flow and a status.
//! - [`GuestApi`]: the few control-API calls the window makes, behind one trait, so
//!   the method names live in one place (the draft names may change in review).

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
        "guest_wrong_session" => "That code belongs to another invite.",
        "guest_bad_code" => "That is not a guest code. Paste the whole code.",
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

/// An error of an API call: the `data.reason` or a short text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError(pub String);

/// The control-API calls of the guest window (draft: `/mnt/project-files/webrtc-guest/
/// control-api-proposal.md`). Ids are the API's: a share id or a guest source id.
pub trait GuestApi {
    /// `guest.share`: starts a share; returns its id.
    fn share(&mut self) -> Result<u64, ApiError>;
    /// `guest.share.code` / `guest.source.answer_code`: the pending code.
    fn code(&mut self, flow: Flow, id: u64) -> Result<String, ApiError>;
    /// `guest.share.accept_answer`.
    fn accept_answer(&mut self, id: u64, answer: &str) -> Result<(), ApiError>;
    /// `guest.source.add`: adds a guest source from an invite; returns its id.
    fn add_guest(&mut self, invite: &str) -> Result<u64, ApiError>;
    /// `guest.share.confirm` / `guest.source.confirm`.
    fn confirm(&mut self, flow: Flow, id: u64) -> Result<(), ApiError>;
    /// `guest.share.stop`, or removing the guest source.
    fn stop(&mut self, flow: Flow, id: u64) -> Result<(), ApiError>;
    /// `guest.status` for one share or guest.
    fn status(&mut self, flow: Flow, id: u64) -> Result<Status, ApiError>;
}

/// The API until the guest methods exist on main: every call says so.
#[derive(Default)]
pub struct Unavailable;

const NOT_YET: &str = "Guests are not available in this build yet.";

impl GuestApi for Unavailable {
    fn share(&mut self) -> Result<u64, ApiError> {
        Err(ApiError(NOT_YET.into()))
    }
    fn code(&mut self, _: Flow, _: u64) -> Result<String, ApiError> {
        Err(ApiError(NOT_YET.into()))
    }
    fn accept_answer(&mut self, _: u64, _: &str) -> Result<(), ApiError> {
        Err(ApiError(NOT_YET.into()))
    }
    fn add_guest(&mut self, _: &str) -> Result<u64, ApiError> {
        Err(ApiError(NOT_YET.into()))
    }
    fn confirm(&mut self, _: Flow, _: u64) -> Result<(), ApiError> {
        Err(ApiError(NOT_YET.into()))
    }
    fn stop(&mut self, _: Flow, _: u64) -> Result<(), ApiError> {
        Err(ApiError(NOT_YET.into()))
    }
    fn status(&mut self, _: Flow, _: u64) -> Result<Status, ApiError> {
        Err(ApiError(NOT_YET.into()))
    }
}

/// Whether an API error is a typed `guest_*` reason (or an encoder-slot refusal) rather
/// than a plain message.
fn is_reason(e: &str) -> bool {
    e.starts_with("guest_") || e == "no_encoder_slot" || e == "too_many_outputs"
}

/// What the window holds between two API calls: the pairing's id, its last status,
/// the code shown and the problem line. The window pushes [`Session::view`].
pub struct Session {
    flow: Flow,
    id: Option<u64>,
    status: Status,
    code: String,
    problem: String,
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
        }
    }

    /// The flow.
    pub fn flow(&self) -> Flow {
        self.flow
    }

    /// The pairing's id once it exists.
    pub fn id(&self) -> Option<u64> {
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

    /// Whether another [`poll`](Self::poll) can change anything: a pairing exists that
    /// has not ended.
    pub fn wants_poll(&self) -> bool {
        self.id.is_some() && !matches!(self.status.state, State::Failed | State::Closed)
    }

    fn fail(&mut self, e: ApiError) {
        if is_reason(&e.0) {
            self.status.state = State::Failed;
            self.status.reason = Some(e.0);
            self.problem.clear();
        } else {
            self.problem = e.0;
        }
    }

    /// Share: asks the API to start a share. Add guest has nothing to ask yet.
    pub fn start(&mut self, api: &mut dyn GuestApi) {
        self.problem.clear();
        if self.flow == Flow::Share {
            match api.share() {
                Ok(id) => self.id = Some(id),
                Err(e) => self.fail(e),
            }
            self.poll(api);
        }
    }

    /// The pasted text: Share's answer code, Add guest's invite code.
    pub fn submit(&mut self, api: &mut dyn GuestApi, text: &str) {
        self.problem.clear();
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let result = match (self.flow, self.id) {
            (Flow::Share, Some(id)) => api.accept_answer(id, text),
            (Flow::Host, None) => api.add_guest(text).map(|id| self.id = Some(id)),
            _ => return,
        };
        match result {
            Ok(()) => self.poll(api),
            Err(ApiError(e)) if is_reason(&e) => self.problem = reason_text(&e).into(),
            Err(e) => self.problem = e.0,
        }
    }

    /// Reads the status, and the code once there is one to show.
    pub fn poll(&mut self, api: &mut dyn GuestApi) {
        let Some(id) = self.id else { return };
        match api.status(self.flow, id) {
            Ok(s) => self.status = s,
            Err(e) => return self.fail(e),
        }
        let has_code = match self.flow {
            Flow::Share => self.status.state == State::WaitingForAnswer,
            Flow::Host => self.status.state == State::Connecting,
        };
        if has_code && self.code.is_empty() {
            match api.code(self.flow, id) {
                Ok(c) => self.code = c,
                Err(e) => self.problem = e.0,
            }
        }
        if matches!(self.status.state, State::CheckPhrase | State::Live) {
            self.code.clear();
        }
    }

    /// The words match.
    pub fn confirm(&mut self, api: &mut dyn GuestApi) {
        self.problem.clear();
        if let Some(id) = self.id {
            match api.confirm(self.flow, id) {
                Ok(()) => self.poll(api),
                Err(e) => self.problem = e.0,
            }
        }
    }

    /// The words differ, or the user stops a running pairing: ends it.
    pub fn stop(&mut self, api: &mut dyn GuestApi) {
        self.problem.clear();
        if let Some(id) = self.id.take()
            && let Err(e) = api.stop(self.flow, id)
        {
            self.problem = e.0;
        }
        self.code.clear();
        self.status = Status {
            state: State::Closed,
            ..Status::gathering()
        };
    }

    /// Start again after a failure or a stop: a fresh session of the same flow.
    pub fn again(&mut self, api: &mut dyn GuestApi) {
        *self = Self::new(self.flow);
        self.start(api);
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

    #[test]
    fn share_invite_waits_with_a_paste_box_and_a_clock() {
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
    fn the_stub_refuses_every_call() {
        let mut a = Unavailable;
        assert!(a.share().is_err() && a.status(Flow::Host, 1).is_err());
    }

    #[test]
    fn clock_pads_seconds() {
        assert_eq!(clock(125), "2:05");
        assert_eq!(clock(0), "0:00");
    }

    /// A scripted API: the status it reports, and a log of the calls.
    struct Fake {
        status: Status,
        log: Vec<String>,
        answer_error: Option<&'static str>,
    }

    impl Fake {
        fn new(state: State) -> Self {
            Self {
                status: st(state),
                log: Vec::new(),
                answer_error: None,
            }
        }
    }

    impl GuestApi for Fake {
        fn share(&mut self) -> Result<u64, ApiError> {
            self.log.push("share".into());
            Ok(7)
        }
        fn code(&mut self, _: Flow, id: u64) -> Result<String, ApiError> {
            self.log.push(format!("code {id}"));
            Ok("rbs-guest:1:abc".into())
        }
        fn accept_answer(&mut self, id: u64, a: &str) -> Result<(), ApiError> {
            self.log.push(format!("answer {id} {a}"));
            match self.answer_error {
                Some(e) => Err(ApiError(e.into())),
                None => Ok(()),
            }
        }
        fn add_guest(&mut self, invite: &str) -> Result<u64, ApiError> {
            self.log.push(format!("add {invite}"));
            Ok(3)
        }
        fn confirm(&mut self, _: Flow, id: u64) -> Result<(), ApiError> {
            self.log.push(format!("confirm {id}"));
            self.status.state = State::Live;
            Ok(())
        }
        fn stop(&mut self, _: Flow, id: u64) -> Result<(), ApiError> {
            self.log.push(format!("stop {id}"));
            Ok(())
        }
        fn status(&mut self, _: Flow, _: u64) -> Result<Status, ApiError> {
            Ok(self.status.clone())
        }
    }

    #[test]
    fn a_share_fetches_its_code_once_it_waits() {
        let mut api = Fake::new(State::WaitingForAnswer);
        let mut s = Session::new(Flow::Share);
        s.start(&mut api);
        s.poll(&mut api);
        assert_eq!(s.code(), "rbs-guest:1:abc");
        assert_eq!(api.log.iter().filter(|c| c.starts_with("code")).count(), 1);
        s.submit(&mut api, "  rbs-guest:1:answer \n");
        assert!(api.log.contains(&"answer 7 rbs-guest:1:answer".to_string()));
    }

    #[test]
    fn a_bad_answer_keeps_the_share_waiting_with_a_problem_line() {
        let mut api = Fake::new(State::WaitingForAnswer);
        api.answer_error = Some("guest_wrong_session");
        let mut s = Session::new(Flow::Share);
        s.start(&mut api);
        s.submit(&mut api, "x");
        assert_eq!(s.problem(), "That code belongs to another invite.");
        assert_eq!(s.view().page, Page::Invite);
        assert!(s.wants_poll());
    }

    #[test]
    fn adding_a_guest_needs_an_invite_first() {
        let mut api = Fake::new(State::Connecting);
        let mut s = Session::new(Flow::Host);
        s.start(&mut api);
        assert!(api.log.is_empty() && s.id().is_none() && !s.wants_poll());
        s.submit(&mut api, "rbs-guest:1:invite");
        assert_eq!(s.id(), Some(3));
        assert_eq!(s.view().page, Page::Answer);
        assert_eq!(s.code(), "rbs-guest:1:abc");
    }

    #[test]
    fn confirming_runs_and_the_code_goes_away() {
        let mut api = Fake::new(State::CheckPhrase);
        let mut s = Session::new(Flow::Share);
        s.start(&mut api);
        assert_eq!(s.view().page, Page::Compare);
        s.confirm(&mut api);
        assert_eq!(s.view().page, Page::Live);
        assert_eq!(s.code(), "");
    }

    #[test]
    fn stop_ends_the_pairing_and_again_starts_a_new_one() {
        let mut api = Fake::new(State::Live);
        let mut s = Session::new(Flow::Share);
        s.start(&mut api);
        s.stop(&mut api);
        assert!(api.log.contains(&"stop 7".to_string()));
        assert_eq!(s.view().page, Page::Failed);
        assert!(!s.wants_poll());
        api.status = st(State::WaitingForAnswer);
        s.again(&mut api);
        assert_eq!(s.view().page, Page::Invite);
    }

    #[test]
    fn a_refused_share_fails_with_its_reason_and_the_stub_shows_a_problem() {
        struct Refuse;
        impl GuestApi for Refuse {
            fn share(&mut self) -> Result<u64, ApiError> {
                Err(ApiError("guest_return_feed".into()))
            }
            fn code(&mut self, _: Flow, _: u64) -> Result<String, ApiError> {
                unreachable!()
            }
            fn accept_answer(&mut self, _: u64, _: &str) -> Result<(), ApiError> {
                unreachable!()
            }
            fn add_guest(&mut self, _: &str) -> Result<u64, ApiError> {
                unreachable!()
            }
            fn confirm(&mut self, _: Flow, _: u64) -> Result<(), ApiError> {
                unreachable!()
            }
            fn stop(&mut self, _: Flow, _: u64) -> Result<(), ApiError> {
                unreachable!()
            }
            fn status(&mut self, _: Flow, _: u64) -> Result<Status, ApiError> {
                unreachable!()
            }
        }
        let mut s = Session::new(Flow::Share);
        s.start(&mut Refuse);
        assert_eq!(s.view().page, Page::Failed);
        assert!(s.view().failure.contains("return mix"));
        let mut s = Session::new(Flow::Share);
        s.start(&mut Unavailable);
        assert_eq!(s.view().page, Page::Invite);
        assert_eq!(s.problem(), NOT_YET);
    }
}
