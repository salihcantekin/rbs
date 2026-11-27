//! Control API answers for the main canvas's buses (protocol 1.115, ADR-0170 decision
//! 9; routed by `ApiTarget::run`). The match is exhaustive: every bus method is here.

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_bus(
        &mut self,
        command: rbs_api::request::BusCommand,
    ) -> Result<Response, ApiError> {
        use rbs_api::request::BusCommand as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::List => Ok(Response::Buses(host.bus_list())),
            C::Add(p) => {
                host.api_add_bus(&p.name)?;
                host.apply(notify);
                Ok(Response::Done(Empty {}))
            }
            C::Remove(p) => {
                host.api_remove_bus(&p.name)?;
                host.apply(notify);
                Ok(Response::Done(Empty {}))
            }
            C::Set(p) => {
                match host.api_set_bus(&p)? {
                    (true, _) => host.apply(notify),
                    (false, true) => host.publish(notify),
                    (false, false) => {}
                }
                Ok(Response::Done(Empty {}))
            }
            C::SetSends(p) => {
                if host.api_set_sends(SourceId::from_raw(p.source), &p.sends)? {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
        }
    }
}
