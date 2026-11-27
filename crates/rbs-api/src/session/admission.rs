//! Shared live authorization and audit for wire and UI-resolved engine calls.

use super::*;
use crate::ui::authorize_engine;

impl Server {
    pub(super) fn admit_engine(
        &mut self,
        conn: ConnectionId,
        id: RequestId,
        command: Command,
    ) -> Queued {
        let Some(session) = self.sessions.get(&conn) else {
            return Queued::Answer(
                id,
                Err(ApiError::new(ErrorKind::NotAuthenticated, "session closed")),
            );
        };
        let Phase::Ready {
            scopes,
            irreversible,
            token,
        } = &session.phase
        else {
            return Queued::Answer(
                id,
                Err(ApiError::new(ErrorKind::NotAuthenticated, "session closed")),
            );
        };
        let caller = Caller {
            token: token.as_ref().map(|t| t.name.clone()),
            irreversible: *irreversible,
            scopes: *scopes,
            remote: !session.peer.is_local(),
        };
        let kind = session.token_kind();
        let method = command.method();
        if let Err(error) = authorize_engine(&caller, &command) {
            if error.kind == ErrorKind::NotPermitted {
                let reason = match error
                    .data
                    .as_ref()
                    .and_then(|d| d.get("reason"))
                    .and_then(|v| v.as_str())
                {
                    Some("scopes") => "scopes",
                    Some("network") => "network",
                    _ => "irreversible",
                };
                self.actions
                    .push_back(Action::Audit(AuditEvent::NotPermitted {
                        conn,
                        token: caller.token,
                        kind,
                        method,
                        reason,
                    }));
            }
            return Queued::Answer(id, Err(error));
        }
        if command.effect().is_irreversible() {
            self.actions
                .push_back(Action::Audit(AuditEvent::Irreversible {
                    conn,
                    token: caller.token.clone(),
                    kind,
                    method,
                }));
        }
        if matches!(
            method_scope_of(method),
            Some(Scope::Admin | Scope::Plugins | Scope::Tokens)
        ) {
            let detail = match &command {
                Command::RecordStartToFile(p) => Some(p.path.clone()),
                Command::TokensCreate(p) => Some(p.name.clone()),
                Command::TokensRevoke(p) => Some(p.name.clone()),
                _ => None,
            };
            self.actions
                .push_back(Action::Audit(AuditEvent::Privileged {
                    conn,
                    token: caller.token,
                    kind,
                    method,
                    detail,
                }));
        }
        Queued::Forward(id, command)
    }

    pub(super) fn admit_ui_engine(
        &mut self,
        conn: ConnectionId,
        id: RequestId,
        command: Command,
        now: Timestamp,
    ) -> Queued {
        if matches!(command, Command::MarkerAdd(_)) && !self.take_marker(conn, now) {
            return Queued::Answer(
                id,
                Err(ApiError::new(
                    ErrorKind::RateLimited,
                    "too many markers: at most 2 a second",
                )),
            );
        }
        self.admit_engine(conn, id, command)
    }
}
