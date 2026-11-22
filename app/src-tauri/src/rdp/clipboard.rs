use ironrdp::{
    cliprdr::{
        CliprdrClient,
        backend::CliprdrBackend,
        pdu::{
            ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags,
            FileContentsRequest, FileContentsResponse, FormatDataRequest, FormatDataResponse,
            LockDataId,
        },
    },
    core::impl_as_any,
    session::ActiveStage,
};

use tokio::time::Instant;

use super::{Error, Event, Input};

pub(super) const MAX_TEXT: usize = 64 * 1024;
const TEXT: ClipboardFormatId = ClipboardFormatId::CF_UNICODETEXT;
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Default)]
enum ReadState {
    #[default]
    Idle,
    Waiting(Instant),
    // FormatDataResponse has no request ID. Drain this reply before sending another request.
    Draining,
}

#[derive(Default)]
pub(super) struct TextClipboard {
    pub(super) ready: bool,
    pub(super) remote_text: bool,
    announced: bool,
    local: Option<String>,
    local_request: Option<ClipboardFormatId>,
    read: ReadState,
    incoming: Option<Result<String, Error>>,
}

impl std::fmt::Debug for TextClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TextClipboard")
    }
}

impl_as_any!(TextClipboard);

impl CliprdrBackend for TextClipboard {
    fn temporary_directory(&self) -> &str {
        ""
    }
    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }
    fn on_ready(&mut self) {
        self.ready = true;
    }
    fn on_request_format_list(&mut self) {
        self.announced = true;
    }
    fn on_process_negotiated_capabilities(&mut self, _: ClipboardGeneralCapabilityFlags) {}
    fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
        self.remote_text = formats.iter().any(|format| format.id() == TEXT);
    }
    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        self.local_request = Some(request.format);
    }
    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        self.expire(Instant::now());
        if matches!(std::mem::take(&mut self.read), ReadState::Waiting(_)) {
            self.incoming = Some(if response.is_error() {
                Err(Error::Clipboard)
            } else if response.data().len() > 2 * (MAX_TEXT + 1) {
                Err(Error::Bounds)
            } else {
                response
                    .to_unicode_string()
                    .map_err(|_| Error::Clipboard)
                    .and_then(|text| {
                        if text.len() <= MAX_TEXT && !text.contains('\0') {
                            Ok(text)
                        } else {
                            Err(Error::Bounds)
                        }
                    })
            });
        }
    }
    fn on_file_contents_request(&mut self, _: FileContentsRequest) {}
    fn on_file_contents_response(&mut self, _: FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _: LockDataId) {}
    fn on_unlock(&mut self, _: LockDataId) {}
}

impl TextClipboard {
    fn expire(&mut self, now: Instant) {
        if matches!(self.read, ReadState::Waiting(deadline) if now >= deadline) {
            self.read = ReadState::Draining;
            self.incoming = Some(Err(Error::Timeout));
        }
    }
}

pub(super) fn deadline(stage: &mut ActiveStage) -> Option<Instant> {
    let state = backend(stage.get_svc_processor_mut::<CliprdrClient>()?);
    match state.read {
        ReadState::Waiting(deadline) => Some(deadline),
        _ => None,
    }
}

pub(super) fn expire(stage: &mut ActiveStage) -> Option<Event> {
    let state = backend(stage.get_svc_processor_mut::<CliprdrClient>()?);
    state.expire(Instant::now());
    state.incoming.take().map(Event::ClipboardText)
}

pub(super) fn backend(channel: &mut CliprdrClient) -> &mut TextClipboard {
    channel
        .downcast_backend_mut::<TextClipboard>()
        .expect("session owns the text clipboard backend")
}

pub(super) fn command(stage: &mut ActiveStage, input: Input) -> Result<Vec<u8>, Error> {
    let channel = stage
        .get_svc_processor_mut::<CliprdrClient>()
        .ok_or(Error::ChannelUnavailable)?;
    if !backend(channel).ready {
        return Err(Error::ChannelUnavailable);
    }
    let messages = match input {
        Input::OfferClipboardText(text) => {
            if text.len() > MAX_TEXT || text.contains('\0') {
                return Err(Error::Bounds);
            }
            let messages = channel
                .initiate_copy(&[ClipboardFormat::new(TEXT)])
                .map_err(|_| Error::Clipboard)?;
            backend(channel).local = Some(text);
            messages
        }
        Input::ReadClipboardText => {
            if !backend(channel).remote_text {
                return Err(Error::ChannelUnavailable);
            }
            if !matches!(backend(channel).read, ReadState::Idle) {
                return Err(Error::Busy);
            }
            let messages = channel.initiate_paste(TEXT).map_err(|_| Error::Clipboard)?;
            backend(channel).read = ReadState::Waiting(Instant::now() + READ_TIMEOUT);
            messages
        }
        _ => unreachable!("clipboard commands only"),
    };
    stage
        .process_svc_processor_messages(messages)
        .map_err(|_| Error::Clipboard)
}

pub(super) fn received(stage: &mut ActiveStage) -> Result<(Vec<u8>, Option<Event>), Error> {
    let Some(channel) = stage.get_svc_processor_mut::<CliprdrClient>() else {
        return Ok((Vec::new(), None));
    };
    let state = backend(channel);
    let event = state.incoming.take().map(Event::ClipboardText);
    let messages = if let Some(format) = state.local_request.take() {
        let response = match (format, state.local.as_deref()) {
            (TEXT, Some(text)) => FormatDataResponse::new_unicode_string(text),
            _ => FormatDataResponse::new_error(),
        };
        channel.submit_format_data(response)
    } else if std::mem::take(&mut state.announced) {
        channel.initiate_copy(&[])
    } else {
        return Ok((Vec::new(), event));
    }
    .map_err(|_| Error::Clipboard)?;
    Ok((
        stage
            .process_svc_processor_messages(messages)
            .map_err(|_| Error::Clipboard)?,
        event,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timeout_drains_old_response_before_accepting_a_new_request() {
        let mut state = TextClipboard {
            read: ReadState::Waiting(Instant::now()),
            ..Default::default()
        };
        let ReadState::Waiting(at) = state.read else {
            unreachable!()
        };
        super::super::wait_deadline(Some(at)).await;
        state.expire(Instant::now());
        assert_eq!(state.incoming.take(), Some(Err(Error::Timeout)));
        state.on_remote_copy(&[ClipboardFormat::new(TEXT)]);
        assert!(matches!(state.read, ReadState::Draining));
        state.on_format_data_response(FormatDataResponse::new_unicode_string("late"));
        assert!(state.incoming.is_none());
        assert!(matches!(state.read, ReadState::Idle));
        state.read = ReadState::Waiting(Instant::now() + READ_TIMEOUT);
        state.on_format_data_response(FormatDataResponse::new_unicode_string("current"));
        assert_eq!(state.incoming.take(), Some(Ok("current".into())));
    }

    #[test]
    fn arrival_at_deadline_never_becomes_clipboard_text() {
        let mut state = TextClipboard {
            read: ReadState::Waiting(Instant::now()),
            ..Default::default()
        };
        state.on_format_data_response(FormatDataResponse::new_unicode_string("late"));
        assert_eq!(state.incoming.take(), Some(Err(Error::Timeout)));
        assert!(matches!(state.read, ReadState::Idle));
    }

    #[test]
    fn oversized_unicode_response_never_reaches_the_local_clipboard() {
        let mut state = TextClipboard {
            read: ReadState::Waiting(Instant::now() + READ_TIMEOUT),
            ..Default::default()
        };
        let text = "한".repeat(MAX_TEXT / 3 + 1);
        state.on_format_data_response(FormatDataResponse::new_unicode_string(&text));
        assert_eq!(state.incoming.take(), Some(Err(Error::Bounds)));
    }
}
