//! The conformance suite every pairing adapter passes (spec "Conformance suite").
//!
//! The suite never sleeps for timing (ADR-0032): the adapter's harness supplies
//! `wait`, which returns once a condition holds (bounded by a hang guard only). Each
//! check panics with a message that names the broken rule. Enabled by the
//! `conformance` feature, from an adapter's dev-dependencies.

#![allow(clippy::expect_used, clippy::panic)] // a conformance check panics by design

use std::time::{SystemTime, UNIX_EPOCH};

use crate::code::{CodeKind, Envelope, SessionId};
use crate::error::{CodeError, PeerError};
use crate::session::{InviteState, PeerEvent, PeerFactory, PeerRole, PeerSession, PeerState};
use crate::settings::{AcceptSettings, InviteSettings, SideSettings};

/// What an adapter's test supplies to the suite.
pub struct Harness<'a> {
    /// The factory under test.
    pub factory: &'a dyn PeerFactory,
    /// Settings with which two pairings on this machine can connect.
    pub side: SideSettings,
    /// Returns once `done` is true; panics (naming the step) at its hang guard.
    pub wait: &'a dyn Fn(&str, &mut dyn FnMut() -> bool),
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn code_error<T>(result: Result<T, PeerError>, what: &str) -> CodeError {
    match result {
        Err(PeerError::Code(e)) => e,
        Err(e) => panic!("{what}: expected a code error, got {e:?}"),
        Ok(_) => panic!("{what}: a bad code was accepted"),
    }
}

/// `accept_invite` refuses a bad code at once, with a typed error, before anything
/// starts: not a code, a broken code, an answer pasted as an invite, an expired one.
pub fn check_bad_invites_are_refused(h: &Harness<'_>) {
    let settings = AcceptSettings::new(h.side.clone());
    let f = h.factory;
    assert_eq!(
        code_error(f.accept_invite("hello", &settings), "not a code"),
        CodeError::NotACode
    );
    code_error(
        f.accept_invite("rbs-guest:1:AAAA", &settings),
        "broken code",
    );
    let sid = SessionId::new([7; 16]).expect("non-zero id");
    let answer = Envelope::new(
        CodeKind::Answer,
        f.transport(),
        sid,
        unix_now() + 600,
        vec![1],
    )
    .encode()
    .expect("small envelope");
    assert_eq!(
        code_error(
            f.accept_invite(answer.expose(), &settings),
            "answer as invite"
        ),
        CodeError::WrongKind
    );
    let expired = Envelope::new(CodeKind::Invite, f.transport(), sid, 1, vec![1])
        .encode()
        .expect("small envelope");
    assert_eq!(
        code_error(
            f.accept_invite(expired.expose(), &settings),
            "expired invite"
        ),
        CodeError::Expired
    );
}

/// Settings out of bounds are refused before a socket or thread exists.
pub fn check_bad_settings_are_refused(h: &Harness<'_>) {
    let mut settings = InviteSettings::new(h.side.clone());
    settings.lifetime = std::time::Duration::ZERO;
    assert!(
        matches!(
            h.factory.create_invite(&settings),
            Err(PeerError::Settings(_))
        ),
        "create_invite: a zero lifetime must be a settings error"
    );
}

/// A full pairing: codes, the same phrase on both sides, each side confirmed only by
/// its own user, the invite used once, the events in order, then close.
pub fn check_pairing(h: &Harness<'_>) {
    let mut invite = h
        .factory
        .create_invite(&InviteSettings::new(h.side.clone()))
        .expect("create_invite");
    (h.wait)("invite code", &mut || {
        invite.state() != InviteState::Gathering
    });
    assert_eq!(
        invite.state(),
        InviteState::Waiting,
        "invite: ready to answer"
    );
    let code = invite
        .code()
        .expect("invite: a waiting invite lends its code");
    assert!(
        !format!("{code:?} {code}").contains(code.expose()),
        "a code never prints its text"
    );
    let invite_code = code.expose().to_owned();

    let mut answerer = h
        .factory
        .accept_invite(&invite_code, &AcceptSettings::new(h.side.clone()))
        .expect("accept_invite");
    assert_eq!(answerer.role(), PeerRole::Answerer);
    assert_eq!(answerer.session_id(), invite.session_id(), "one session id");
    (h.wait)("answer code", &mut || answerer.answer_code().is_some());
    let answer_code = answerer.answer_code().expect("answer").expose().to_owned();

    let mut offerer = invite.accept_answer(&answer_code).expect("accept_answer");
    assert_eq!(offerer.role(), PeerRole::Offerer);
    assert_eq!(invite.state(), InviteState::Used, "invite: used once");
    assert!(
        matches!(
            invite.accept_answer(&answer_code),
            Err(PeerError::AlreadyUsed)
        ),
        "invite: a second answer is refused"
    );
    assert!(
        offerer.answer_code().is_none(),
        "only the answerer has an answer code"
    );

    (h.wait)("both connected", &mut || {
        offerer.check_phrase().is_some() && answerer.check_phrase().is_some()
    });
    assert_eq!(
        offerer.check_phrase(),
        answerer.check_phrase(),
        "one phrase"
    );
    for s in [&offerer, &answerer] {
        assert_eq!(
            s.state(),
            PeerState::Connected { confirmed: false },
            "the check is on: not confirmed before its user confirms"
        );
    }
    offerer.confirm_phrase().expect("confirm offerer");
    assert_eq!(offerer.state(), PeerState::Connected { confirmed: true });
    assert_eq!(
        answerer.state(),
        PeerState::Connected { confirmed: false },
        "each side confirms only itself"
    );
    answerer.confirm_phrase().expect("confirm answerer");
    assert_eq!(answerer.state(), PeerState::Connected { confirmed: true });

    let phrase = offerer.check_phrase().expect("phrase");
    let events =
        |s: &mut Box<dyn PeerSession>| std::iter::from_fn(|| s.poll_event()).collect::<Vec<_>>();
    assert_eq!(
        events(&mut offerer),
        vec![PeerEvent::Connected { phrase }, PeerEvent::Confirmed],
        "offerer events"
    );
    assert_eq!(
        events(&mut answerer),
        vec![
            PeerEvent::AnswerReady,
            PeerEvent::Connected { phrase },
            PeerEvent::Confirmed
        ],
        "answerer events"
    );

    // The receiving half (guest-source.md): once, and only on the answering side.
    assert!(
        offerer.take_receiver().is_none(),
        "an offering session gives no receiver"
    );
    if let Some(receiver) = answerer.take_receiver() {
        assert!(
            answerer.take_receiver().is_none(),
            "the receiver is given once"
        );
        (h.wait)("receiver confirmed", &mut || receiver.confirmed());
        assert!(!receiver.ended());
    }

    offerer.close();
    assert_eq!(offerer.state(), PeerState::Closed, "closed by this side");
    assert_eq!(
        events(&mut offerer),
        vec![PeerEvent::Closed],
        "one Closed event"
    );
    assert!(
        offerer.confirm_phrase().is_err(),
        "a closed session refuses calls"
    );
    answerer.close();
}

/// An answer made for another invite is refused, and the invite still takes its own.
pub fn check_answer_for_another_invite_is_refused(h: &Harness<'_>) {
    let settings = InviteSettings::new(h.side.clone());
    let mut first = h.factory.create_invite(&settings).expect("first invite");
    let mut second = h.factory.create_invite(&settings).expect("second invite");
    (h.wait)("two invite codes", &mut || {
        first.code().is_some() && second.code().is_some()
    });
    let code = second.code().expect("second code").expose().to_owned();
    let mut answerer = h
        .factory
        .accept_invite(&code, &AcceptSettings::new(h.side.clone()))
        .expect("accept second");
    (h.wait)("answer code", &mut || answerer.answer_code().is_some());
    let answer = answerer.answer_code().expect("answer").expose().to_owned();
    assert_eq!(
        code_error(first.accept_answer(&answer), "answer for another invite"),
        CodeError::WrongSession
    );
    assert_eq!(
        first.state(),
        InviteState::Waiting,
        "a refused answer does not use the invite"
    );
    first.close();
    assert_eq!(first.state(), InviteState::Closed);
    assert!(
        first.accept_answer(&answer).is_err(),
        "a closed invite takes no answer"
    );
    second.close();
    answerer.close();
}

/// Every check, in order.
pub fn run_all(h: &Harness<'_>) {
    check_bad_invites_are_refused(h);
    check_bad_settings_are_refused(h);
    check_pairing(h);
    check_answer_for_another_invite_is_refused(h);
}
