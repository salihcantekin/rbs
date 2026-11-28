//! Control API answers for guest shares (protocol 1.116, ADR-0139 decision 14;
//! routed by `ApiTarget::run`). The match is exhaustive: every guest method is here.
//! Codes go only into `guest.share.code`'s answer, which only `admin` tokens reach.

use std::time::Duration;

use rbs_api::request::{GuestCommand, GuestShareRow, GuestState, GuestStatus};

use super::*;
use crate::studio_host::guest_share::{
    ShareRefusal, ShareRequest, ShareState, ShareStatus, ShareVideo,
};

impl ApiTarget<'_> {
    pub(super) fn answer_guest(&mut self, command: GuestCommand) -> Result<Response, ApiError> {
        let host = &mut *self.host;
        let answer = match command {
            GuestCommand::Share(p) => {
                let mut request = ShareRequest {
                    lifetime: p.lifetime_s.map(|s| Duration::from_secs(u64::from(s))),
                    ..ShareRequest::default()
                };
                request.check_phrase = p.check_phrase.unwrap_or(true);
                request.local_only = p.local_only.unwrap_or(false);
                if let Some(v) = p.video {
                    request.video = ShareVideo {
                        width: v.width,
                        height: v.height,
                        bitrate_bps: u64::from(v.bitrate_kbps) * 1000,
                    };
                }
                let share = host.guest_share(&request).map_err(refusal)?;
                Response::GuestShareStarted(rbs_api::request::GuestShareStarted {
                    share,
                    state: GuestState::Gathering,
                })
            }
            GuestCommand::ShareCode(p) => {
                let (code, expires_at) = host.guests.shares.code(p.share).map_err(refusal)?;
                return Ok(Response::GuestCode(rbs_api::request::GuestCode {
                    code,
                    expires_at,
                }));
            }
            GuestCommand::AcceptAnswer(p) => {
                let state = host
                    .guests
                    .shares
                    .accept_answer(p.share, &p.answer)
                    .map_err(refusal)?;
                host.guests.due = Some(host.clock.now());
                Response::GuestShareState(rbs_api::request::GuestShareState {
                    state: api_state(state),
                })
            }
            GuestCommand::Confirm(p) => {
                let state = host.guests.shares.confirm(p.share).map_err(refusal)?;
                host.guests.due = Some(host.clock.now());
                Response::GuestShareState(rbs_api::request::GuestShareState {
                    state: api_state(state),
                })
            }
            GuestCommand::Stop(p) => {
                host.guest_share_stop(p.share).map_err(refusal)?;
                Response::Done(Empty {})
            }
            GuestCommand::Status => {
                return Ok(Response::GuestStatus(api::Snapshot {
                    seq: {
                        host.guest_changed();
                        host.events.seq_map([Topic::GuestChanged])
                    },
                    data: api_guest_status(&host.guests.shares.status()),
                }));
            }
        };
        host.guest_changed();
        Ok(answer)
    }
}

/// The API's state of a share.
fn api_state(state: ShareState) -> GuestState {
    match state {
        ShareState::Gathering => GuestState::Gathering,
        ShareState::WaitingForAnswer => GuestState::WaitingForAnswer,
        ShareState::Connecting => GuestState::Connecting,
        ShareState::CheckPhrase => GuestState::CheckPhrase,
        ShareState::Live => GuestState::Live,
        ShareState::Failed => GuestState::Failed,
        ShareState::Closed => GuestState::Closed,
    }
}

/// `guest.status` and `guest.changed`: never a code.
pub(in crate::studio_host) fn api_guest_status(shares: &[ShareStatus]) -> GuestStatus {
    GuestStatus {
        shares: shares
            .iter()
            .map(|s| GuestShareRow {
                share: s.id,
                state: api_state(s.state),
                phrase: s.phrase.clone(),
                reason: s.reason.map(|r| r.id().to_owned()),
                expires_at: s.expires_at,
                width: s.video.width,
                height: s.video.height,
                bitrate_kbps: u32::try_from(s.video.bitrate_bps / 1000).unwrap_or(u32::MAX),
            })
            .collect(),
    }
}

fn with_reason(kind: ErrorKind, text: String, reason: &str) -> ApiError {
    let mut data = serde_json::Map::new();
    data.insert("reason".into(), reason.into());
    ApiError::with_data(kind, text, data)
}

/// A share refusal as an API error; `data.reason` carries a `guest_*` or slot id.
fn refusal(r: ShareRefusal) -> ApiError {
    match r {
        ShareRefusal::Unavailable => ApiError::new(
            ErrorKind::Conflict,
            "guest sharing is not available in this build",
        ),
        ShareRefusal::NotFound => ApiError::new(ErrorKind::NotFound, "no such share"),
        ShareRefusal::Conflict(text) => ApiError::new(ErrorKind::Conflict, text),
        ShareRefusal::Invalid { text, reason } => match reason {
            Some(reason) => with_reason(ErrorKind::InvalidParams, text, reason),
            None => ApiError::new(ErrorKind::InvalidParams, text),
        },
        ShareRefusal::Reason(reason) => {
            with_reason(ErrorKind::Conflict, reason.text(), reason.id())
        }
        ShareRefusal::Slot { text, reason } => with_reason(ErrorKind::Conflict, text, reason),
        ShareRefusal::Failed(text) => failed(text),
    }
}
