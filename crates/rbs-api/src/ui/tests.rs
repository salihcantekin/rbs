#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::sync::mpsc;

fn call(client: Option<&str>, serial: u64) -> UiForwarded {
    UiForwarded {
        ticket: UiTicket {
            serial,
            conn: crate::transport::ConnectionId::from_raw(1),
            active: Arc::new(AtomicBool::new(true)),
            cancellation: Arc::default(),
        },
        caller: Caller::local(),
        request: UiRequest::Run {
            id: "stream.toggle".into(),
            client: client.map(str::to_owned),
        },
    }
}

fn responder() -> (UiResponder, mpsc::Receiver<UiResolved>) {
    let (tx, rx) = mpsc::channel();
    (
        UiResponder {
            send: Arc::new(move |value| tx.send(value).unwrap()),
            sent: Arc::new(AtomicBool::new(false)),
        },
        rx,
    )
}

#[test]
fn explicit_targets_do_not_change_focus_or_fall_back() {
    let router = UiRouter::default();
    let (tx, rx) = mpsc::channel();
    for id in ["first", "second"] {
        let tx = tx.clone();
        router
            .register(id.into(), id.into(), move |call, reply| {
                tx.send((id, call, reply)).unwrap();
            })
            .unwrap();
    }
    let (reply, answers) = responder();
    router.route(call(None, 1), reply);
    let UiResolution::Complete(Err(error)) = answers.try_recv().unwrap().resolution else {
        panic!("unfocused clients must not receive a request");
    };
    assert_eq!(error.kind, ErrorKind::NotFound);
    router.focus("first").unwrap();
    for (client, expected, serial) in [(Some("second"), "second", 2), (None, "first", 3)] {
        let (reply, _) = responder();
        router.route(call(client, serial), reply);
        assert_eq!(rx.try_recv().unwrap().0, expected);
    }
    let (reply, answers) = responder();
    router.route(call(Some("missing"), 4), reply);
    let UiResolution::Complete(Err(error)) = answers.try_recv().unwrap().resolution else {
        panic!("a missing explicit target must not fall back");
    };
    assert_eq!(error.kind, ErrorKind::NotFound);
    assert!(rx.try_recv().is_err());
    assert_eq!(
        router
            .register("first".into(), "duplicate".into(), |_, _| {})
            .unwrap_err()
            .kind,
        ErrorKind::Conflict,
    );
}

#[test]
fn unregister_cancels_queued_work_and_a_reply_is_single_use() {
    let router = UiRouter::default();
    let (tx, rx) = mpsc::channel();
    router
        .register("desktop".into(), "Desktop".into(), move |call, reply| {
            tx.send((call, reply)).unwrap();
        })
        .unwrap();
    let (reply, answers) = responder();
    router.route(call(Some("desktop"), 1), reply);
    let (call, reply) = rx.try_recv().unwrap();
    router.unregister("desktop");
    assert!(!call.ticket.is_active());
    let UiResolution::Complete(Err(error)) = answers.try_recv().unwrap().resolution else {
        panic!("closed clients must answer their pending calls");
    };
    assert_eq!(error.kind, ErrorKind::Conflict);
    reply.send(call.resolve(UiResolution::Engine(Command::SourceList)));
    assert!(answers.try_recv().is_err());
}

#[test]
fn a_cancelled_call_never_reaches_the_client_sink() {
    let router = UiRouter::default();
    let (tx, rx) = mpsc::channel();
    router
        .register("desktop".into(), "Desktop".into(), move |_, _| {
            tx.send(()).unwrap();
        })
        .unwrap();
    let call = call(Some("desktop"), 1);
    call.ticket.cancel();
    let (reply, _) = responder();
    router.route(call, reply);
    assert!(rx.try_recv().is_err());
}

#[test]
fn cancellation_observers_run_once_and_late_registration_is_not_lost() {
    use std::sync::atomic::AtomicUsize;
    for early in [false, true] {
        let call = call(Some("desktop"), 1);
        let count = Arc::new(AtomicUsize::new(0));
        if early {
            call.ticket.cancel();
        }
        let observed = count.clone();
        call.ticket
            .on_cancel(move || {
                observed.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();
        call.ticket.cancel();
        call.ticket.cancel();
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn resolving_engine_work_keeps_its_cancellation_lease_until_completion() {
    let router = UiRouter::default();
    router
        .register("desktop".into(), "Desktop".into(), |call, reply| {
            reply.send(call.resolve(UiResolution::Engine(Command::SourceList)));
        })
        .unwrap();
    let first = call(Some("desktop"), 1);
    for call in [first.clone(), call(Some("desktop"), 2)] {
        let (reply, _answers) = responder();
        // Keep the answer receiver alive while the synchronous sink runs.
        router.route(call, reply);
    }
    router.unregister("desktop");
    assert!(
        !first.ticket.is_active(),
        "a later route must not prune the running gesture"
    );
}

#[test]
fn cancelled_undelivered_callbacks_cannot_grow_the_ui_queue_without_bound() {
    let router = UiRouter::default();
    router
        .register("desktop".into(), "Desktop".into(), |_, _| {})
        .unwrap();
    for serial in 1..=128 {
        let call = call(Some("desktop"), serial);
        let (reply, _answers) = responder();
        router.route(call.clone(), reply);
        call.ticket.cancel();
    }
    let (reply, answers) = responder();
    router.route(call(Some("desktop"), 129), reply);
    let UiResolution::Complete(Err(error)) = answers.try_recv().unwrap().resolution else {
        panic!("the bounded queue must refuse more UI work");
    };
    assert_eq!(error.kind, ErrorKind::Busy);
}
