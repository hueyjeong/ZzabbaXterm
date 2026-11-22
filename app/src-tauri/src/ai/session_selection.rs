use std::{num::NonZeroU64, sync::Mutex};

use serde::{Deserialize, Serialize};
use tauri::{
    Manager, Runtime,
    ipc::{Invoke, InvokeBody, InvokeResponseBody},
    utils::acl::ExecutionContext,
};

use super::approval::LocalReview;
use crate::terminal::{LiveSsmIdentity, Sessions};

const OWNER: &str = "main";
const READ: &str = "ai_session_selection";
const SELECT: &str = "ai_session_select";
const CONFIRM: &str = "ai_session_confirm";
const CLEAR: &str = "ai_session_clear";
const INVALID: &str = "AI 세션 선택 요청 형식이 올바르지 않습니다";
const STALE: &str = "AI 세션 선택 또는 문서가 바뀌었습니다";
const UNAVAILABLE: &str = "확인된 SSM 세션을 사용할 수 없습니다";
const LOCAL_ONLY: &str = "앱의 로컬 기본 창에서만 AI 세션을 선택할 수 있습니다";

#[derive(Default)]
pub(crate) struct State(Mutex<SelectionState>);

#[derive(Default)]
struct SelectionState {
    last_id: u64,
    selected: Option<Selection>,
}

struct Selection {
    selection_id: NonZeroU64,
    identity: LiveSsmIdentity,
    confirmed: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct SelectionView<'a> {
    id: &'a str,
    region: &'a str,
    node: &'a str,
    selection_id: String,
    confirmed: bool,
}

impl Selection {
    fn view(&self) -> SelectionView<'_> {
        SelectionView {
            id: &self.identity.id,
            region: &self.identity.region,
            node: &self.identity.instance_id,
            selection_id: self.selection_id.to_string(),
            confirmed: self.confirmed,
        }
    }

    fn check(&self, document: u64, selection_id: NonZeroU64, id: Option<&str>) -> Result<(), &'static str> {
        if self.identity.document != document || self.selection_id != selection_id
            || id.is_some_and(|id| id != self.identity.id)
        {
            return Err(STALE);
        }
        Ok(())
    }
}

impl SelectionState {
    fn replace(&mut self, identity: LiveSsmIdentity) -> Result<(), &'static str> {
        let selection_id = self.last_id.checked_add(1).and_then(NonZeroU64::new).ok_or(STALE)?;
        self.last_id = selection_id.get();
        self.selected = Some(Selection { selection_id, identity, confirmed: false });
        Ok(())
    }
}

impl State {
    pub(crate) fn document_changed(&self, owner: &str, current_document: u64) {
        if owner != OWNER {
            return;
        }
        let mut state = self.0.lock().unwrap();
        if state.selected.as_ref().is_some_and(|selection| selection.identity.document < current_document) {
            state.selected = None;
        }
    }

    fn handle(&self, sessions: &Sessions, document: u64, request: Request<'_>) -> Result<InvokeResponseBody, String> {
        // 문서 수명 호출은 Sessions의 잠금을 놓은 뒤에 이 상태를 정리한다.
        let mut state = self.0.lock().unwrap();
        match request {
            Request::Select { id } => {
                let identity = sessions.live_ssm_identity(OWNER, document, id).ok_or(UNAVAILABLE)?;
                sessions.with_live_ssm_session(OWNER, &identity, || state.replace(identity.clone())).ok_or(UNAVAILABLE)??;
            }
            Request::Read => {
                let Some(selection) = &state.selected else {
                    return sessions.with_probes(OWNER, document, |_| Ok(null_response()));
                };
                if selection.identity.document != document {
                    return Err(STALE.into());
                }
                sessions.with_live_ssm_session(OWNER, &selection.identity, || ()).ok_or(UNAVAILABLE)?;
            }
            Request::Confirm { id, selection_id } => {
                let selection = state.selected.as_mut().ok_or(STALE)?;
                selection.check(document, selection_id, Some(id))?;
                let confirmed = &mut selection.confirmed;
                sessions.with_live_ssm_session(OWNER, &selection.identity, || *confirmed = true).ok_or(UNAVAILABLE)?;
            }
            Request::Clear { selection_id } => {
                state.selected.as_ref().ok_or(STALE)?.check(document, selection_id, None)?;
                return sessions.with_probes(OWNER, document, |_| {
                    state.selected = None;
                    Ok(null_response())
                });
            }
        }
        let selection = state.selected.as_ref().ok_or(STALE)?;
        serde_json::to_string(&selection.view()).map(InvokeResponseBody::Json).map_err(|_| INVALID.into())
    }
}

fn null_response() -> InvokeResponseBody {
    InvokeResponseBody::Json("null".into())
}

#[derive(Debug, PartialEq, Eq)]
enum Request<'a> {
    Read,
    Select { id: &'a str },
    Confirm { id: &'a str, selection_id: NonZeroU64 },
    Clear { selection_id: NonZeroU64 },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Select<'a> { id: &'a str }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Confirm<'a> { id: &'a str, selection_id: &'a str }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Clear<'a> { selection_id: &'a str }

fn parse_selection_id(value: &str) -> Result<NonZeroU64, &'static str> {
    if value.starts_with('0') || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(INVALID);
    }
    value.parse().map_err(|_| INVALID)
}

fn parse<'a>(command: &str, body: &'a InvokeBody) -> Result<Request<'a>, &'static str> {
    let InvokeBody::Json(value) = body else { return Err(INVALID) };
    let fields = value.as_object().ok_or(INVALID)?;
    match command {
        READ if fields.is_empty() => Ok(Request::Read),
        SELECT => {
            let Select { id } = Select::deserialize(value).map_err(|_| INVALID)?;
            Ok(Request::Select { id })
        }
        CONFIRM => {
            let Confirm { id, selection_id } = Confirm::deserialize(value).map_err(|_| INVALID)?;
            Ok(Request::Confirm { id, selection_id: parse_selection_id(selection_id)? })
        }
        CLEAR => {
            let Clear { selection_id } = Clear::deserialize(value).map_err(|_| INVALID)?;
            Ok(Request::Clear { selection_id: parse_selection_id(selection_id)? })
        }
        _ => Err(INVALID),
    }
}

pub(crate) fn handles(command: &str) -> bool {
    matches!(command, READ | SELECT | CONFIRM | CLEAR)
}

pub(crate) fn dispatch<R: Runtime>(invoke: Invoke<R>) {
    let caller = invoke.message.webview_ref();
    let sessions = caller.state::<Sessions>();
    let document = sessions.document(caller.label());
    let has_local_acl = invoke.acl.as_ref().is_some_and(|acl| {
        acl.iter().any(|command| matches!(command.context, ExecutionContext::Local))
    });
    if LocalReview::check(caller.label(), caller.window_ref().label(), has_local_acl, document).is_err() {
        invoke.resolver.reject(LOCAL_ONLY);
        return;
    }
    let result = parse(invoke.message.command(), invoke.message.payload())
        .map_err(str::to_owned)
        .and_then(|request| caller.state::<State>().handle(&sessions, document, request));
    match result {
        Ok(value) => invoke.resolver.resolve(value),
        Err(error) => invoke.resolver.reject(error),
    }
}

#[cfg(test)]
#[path = "session_selection/tests.rs"]
mod tests;
