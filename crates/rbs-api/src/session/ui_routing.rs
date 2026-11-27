use super::*;
use crate::ui::{UiForwarded, UiRequest, UiResolution, UiResolved, UiTicket, authorize_engine};
use std::sync::atomic::AtomicBool;

pub(super) struct PendingUi {
    pub(super) ticket: UiTicket,
    id: RequestId,
    request: UiRequest,
}

impl Server {
    pub(super) fn forward_ui(&mut self, conn: ConnectionId, id: RequestId, request: UiRequest) {
        let Some(session) = self.sessions.get_mut(&conn) else {
            return;
        };
        let Phase::Ready {
            scopes,
            irreversible,
            token,
        } = &session.phase
        else {
            return;
        };
        self.ui_serial = self.ui_serial.wrapping_add(1);
        let ticket = UiTicket {
            serial: self.ui_serial,
            conn,
            active: Arc::new(AtomicBool::new(true)),
            cancellation: Arc::default(),
        };
        let caller = Caller {
            token: token.as_ref().map(|token| token.name.clone()),
            irreversible: *irreversible,
            scopes: *scopes,
            remote: !session.peer.is_local(),
        };
        session.in_flight = true;
        session.in_flight_method = Some(request.method());
        session.pending_ui = Some(PendingUi {
            ticket: ticket.clone(),
            id,
            request: request.clone(),
        });
        self.actions.push_back(Action::Ui(UiForwarded {
            ticket,
            caller,
            request,
        }));
    }

    pub(super) fn resolve_ui(&mut self, resolved: UiResolved, now: Timestamp) {
        let conn = resolved.ticket.conn;
        let Some(session) = self.sessions.get_mut(&conn) else {
            return;
        };
        if !session
            .pending_ui
            .as_ref()
            .is_some_and(|pending| pending.ticket == resolved.ticket)
        {
            return;
        }
        let Phase::Ready {
            scopes,
            irreversible,
            token,
        } = &session.phase
        else {
            return;
        };
        let caller = Caller {
            token: token.as_ref().map(|token| token.name.clone()),
            irreversible: *irreversible,
            scopes: *scopes,
            remote: !session.peer.is_local(),
        };
        let Some(pending) = session.pending_ui.take() else {
            return;
        };
        let active = pending.ticket.is_active();
        if !active {
            self.reply(
                Reply {
                    conn,
                    id: pending.id,
                    result: Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the UI request was cancelled",
                    )),
                },
                now,
            );
            return;
        }
        session.ui_ticket = Some(pending.ticket);
        match resolved.resolution {
            UiResolution::Complete(result) => {
                self.reply(
                    Reply {
                        conn,
                        id: pending.id,
                        result,
                    },
                    now,
                );
            }
            UiResolution::Engine(command) => {
                if matches!(pending.request, UiRequest::List { .. }) {
                    self.reply(
                        Reply {
                            conn,
                            id: pending.id,
                            result: Err(ApiError::new(
                                ErrorKind::InvalidRequest,
                                "a registry listing cannot execute an engine command",
                            )),
                        },
                        now,
                    );
                } else {
                    self.queue_ui_engine(conn, pending.id, command, now);
                }
            }
            UiResolution::Batch(commands) => {
                let check = if commands.is_empty()
                    || commands.len() > 128
                    || matches!(pending.request, UiRequest::List { .. })
                {
                    Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "invalid UI operation sequence",
                    ))
                } else {
                    let mut result = Ok(());
                    for command in &commands {
                        if let Err(error) = authorize_engine(&caller, command) {
                            let _ = self.admit_engine(conn, pending.id.clone(), command.clone());
                            result = Err(error);
                            break;
                        }
                    }
                    result
                };
                match check {
                    Err(error) => {
                        if let Some(session) = self.sessions.get_mut(&conn) {
                            session.in_flight_method = None;
                        }
                        self.reply(
                            Reply {
                                conn,
                                id: pending.id,
                                result: Err(error),
                            },
                            now,
                        );
                    }
                    Ok(()) => {
                        if let Some(session) = self.sessions.get_mut(&conn) {
                            session.ui_commands = commands.into();
                        }
                        let result = Ok(Response::Done(Empty {}));
                        if !self.continue_ui(conn, &pending.id, &result, now) {
                            self.reply(
                                Reply {
                                    conn,
                                    id: pending.id,
                                    result,
                                },
                                now,
                            );
                        }
                    }
                }
            }
        }
    }

    pub(super) fn continue_ui(
        &mut self,
        conn: ConnectionId,
        id: &RequestId,
        result: &Result<Response, ApiError>,
        now: Timestamp,
    ) -> bool {
        let Some(session) = self.sessions.get_mut(&conn) else {
            return false;
        };
        if result.is_err()
            || !matches!(session.phase, Phase::Ready { .. })
            || session
                .ui_ticket
                .as_ref()
                .is_some_and(|ticket| !ticket.is_active())
        {
            session.ui_commands.clear();
            return false;
        }
        let Some(command) = session.ui_commands.pop_front() else {
            return false;
        };
        self.queue_ui_engine(conn, id.clone(), command, now);
        true
    }

    fn queue_ui_engine(
        &mut self,
        conn: ConnectionId,
        id: RequestId,
        command: Command,
        now: Timestamp,
    ) {
        match self.admit_ui_engine(conn, id, command, now) {
            Queued::Answer(id, result) => {
                if let Some(session) = self.sessions.get_mut(&conn) {
                    session.in_flight_method = None;
                }
                self.reply(Reply { conn, id, result }, now);
            }
            queued => {
                if let Some(session) = self.sessions.get_mut(&conn) {
                    session.in_flight = false;
                    session.in_flight_method = None;
                    session.queue.push_front(queued);
                }
                self.pump(conn, now);
            }
        }
    }
}
