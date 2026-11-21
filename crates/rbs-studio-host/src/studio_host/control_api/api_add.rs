//! Control API answers for the additions of protocol 1.100: the voice chain and the
//! stinger file (ADR-0164 decision 6; routed by `ApiTarget::run`).

use super::*;

impl ApiTarget<'_> {
    pub(super) fn answer_add(&mut self, command: rbs_api::Command) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        let notify = self.notify;
        let host = &mut *self.host;
        match command {
            C::FilterAddVoiceChain(p) => {
                if host
                    .add_voice_chain(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CollectionSetStingerFile(p) => {
                let path = std::path::Path::new(&p.path);
                if path.is_relative() {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "The stinger file needs an absolute path.",
                    ));
                }
                let id = host.set_stinger_file(path).map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::SourceAdded(SourceAdded { source: id.get() }))
            }
            // Routed elsewhere by `ApiTarget::run`.
            _ => Err(rbs_api::control::unsupported(&command)),
        }
    }
}
