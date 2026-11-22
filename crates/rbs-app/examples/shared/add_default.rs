//! Adds a kind's default target as the window does (ADR-0168 step 4): `source.targets`
//! by ticket, then `source.add`. Shared by the examples that need a screen and a
//! microphone.

use std::sync::mpsc;
use std::time::Duration;

use rbs_app::studio_host::{
    ApiReply, Command, SourceKind, StudioHost, Target, Update, kind_for_api,
};

/// Adds `kind`'s default target, else its first one; says what it added.
pub fn add_default(
    host: &StudioHost,
    rx: &mpsc::Receiver<Update>,
    kind: SourceKind,
    wait: Duration,
) {
    let Some(api_kind) = kind_for_api(kind) else {
        return;
    };
    host.send(Command::Api {
        call: Box::new(rbs_api::Command::SourceTargets(
            rbs_api::request::TargetsParams { kind: api_kind },
        )),
        reply: ApiReply::Ticket(1),
    });
    let target = loop {
        match rx.recv_timeout(wait) {
            Ok(Update::ApiAnswer {
                ticket: 1,
                result: Ok(rbs_api::Response::Targets(list)),
            }) => {
                let t: Vec<Target> = list.targets.into_iter().map(Target::from_api).collect();
                break t
                    .iter()
                    .find(|t| t.is_default && !t.picker)
                    .cloned()
                    .or_else(|| t.into_iter().find(|t| !t.picker));
            }
            Ok(_) => {}
            Err(_) => break None,
        }
    };
    let Some(target) = target else {
        rbs_crash::outln!("no {kind:?} to add");
        return;
    };
    rbs_crash::outln!("adding {kind:?}: {}", target.name);
    host.send(Command::Api {
        call: Box::new(rbs_api::Command::SourceAdd(rbs_api::request::AddSource {
            kind: api_kind,
            adapter: target.adapter,
            target: target.target,
            name: None,
            scene: None,
            global: false,
            with_audio: false,
            text: None,
            stamp: None,
            path: None,
            url: None,
        })),
        reply: ApiReply::Shown,
    });
}
