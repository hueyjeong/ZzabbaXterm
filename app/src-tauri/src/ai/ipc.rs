use std::{num::NonZeroU64, sync::{Arc, Mutex}, time::Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{Manager, Webview, ipc::{Invoke, InvokeBody, InvokeError}};
use tokio_util::sync::CancellationToken;

use crate::{connections, terminal::Sessions};
use super::{ChatMessage, ChatRole, Ollama, Operation, SelectedTarget, UPTIME,
    approval::{Candidate, Card, Choice, Decision as RecordedDecision, DecisionRequest, LocalReview, Policy, Reviews},
    execution, session_selection};
use super::ReviewCandidate;
use super::cli::{Proposal as CliProposal, Query, Rejected};
const STALE: &str = "조회 대상 또는 문서가 바뀌었습니다";
const INVALID: &str = "AI 요청 형식이 올바르지 않습니다";
const CANCELLED: &str = "제안 요청이 취소되었습니다";
const EXEC_SESSION: &str = "확인된 AI 작업 세션과 실행 승인이 일치하지 않습니다";
const EXEC_APPROVAL: &str = "실행 승인을 다시 받아야 합니다";
const EXEC_POLICY: &str = "정책이 바뀌어 실행 승인을 사용할 수 없습니다";
type Result<T, E = String> = std::result::Result<T, E>;

#[derive(Default)]
pub(crate) struct State(Mutex<ReviewState>);

impl State {
    pub(super) fn receive_proposal(&self, targets: &[SelectedTarget], request: CliProposal, deadline: Instant) -> Result<String, Rejected> {
        let CliProposal { target_id, generation, operation: Query::Uptime } = request;
        let target = TargetRef { target_id, generation };
        let selected = targets.iter().find(|saved| target.matches(saved)).ok_or(Rejected)?;
        let mut state = self.0.lock().unwrap();
        if Instant::now() >= deadline { return Err(Rejected); }
        let policy = state.policy;
        if let Some(id) = state.reviews.cli_id(selected, policy) {
            return Ok(id.to_string());
        }
        state.reviews.propose(Candidate::Cli { target_id: &selected.profile_id, operation: UPTIME }, Some(selected), policy)
            .map(|id| id.to_string()).map_err(|_| Rejected)
    }
}

#[derive(Default)]
struct ReviewState {
    selection: Option<Selection>,
    epoch: u64,
    chat: Option<ChatRequestId>,
    cancelled_through: Option<(u64, u32)>,
    reviews: Reviews,
    policy: Policy,
}

struct Selection {
    document: u64,
    target: SelectedTarget,
}

// 문서 안에서 증가하는 ID로 시작보다 먼저 도착한 취소도 보존한다.
struct ChatRequestId {
    document: u64,
    latest: u32,
    token: Option<CancellationToken>,
}

impl ReviewState {
    fn set_policy(&mut self, blocked: bool) -> Result<()> {
        if self.policy.blocked != blocked {
            let revision = self.policy.revision.checked_add(1).ok_or(STALE)?;
            self.epoch = self.epoch.checked_add(1).ok_or(STALE)?;
            self.policy = Policy { revision, blocked };
            self.reviews.clear_pending();
            if let Some(token) = self.chat.as_mut().and_then(|chat| chat.token.take()) {
                token.cancel();
            }
        }
        Ok(())
    }

    fn select(&mut self, document: u64, target: Option<SelectedTarget>) -> Result<()> {
        self.epoch = self.epoch.checked_add(1).ok_or(STALE)?;
        self.reviews.clear_open();
        if let Some(token) = self.chat.as_mut().and_then(|chat| chat.token.take()) {
            token.cancel();
        }
        self.selection = target.map(|target| Selection { document, target });
        Ok(())
    }

    fn selected<'a>(&self, document: u64, targets: &'a [SelectedTarget]) -> Result<&'a SelectedTarget> {
        let selection = self.selection.as_ref().filter(|value| value.document == document).ok_or(STALE)?;
        targets.iter().find(|target| *target == &selection.target).ok_or_else(|| STALE.into())
    }

    fn begin(&mut self, document: u64, targets: &[SelectedTarget], request_id: u32, target: TargetRef) -> Result<(u64, SelectedTarget, CancellationToken)> {
        let selected = self.selected(document, targets)?;
        if !target.matches(selected) {
            return Err(STALE.into());
        }
        if self.chat.as_ref().is_some_and(|chat| chat.document == document && request_id <= chat.latest)
            || self.cancelled_through.is_some_and(|(owner, id)| owner == document && request_id <= id)
        {
            return Err(CANCELLED.into());
        }
        let token = CancellationToken::new();
        if let Some(previous) = self.chat.replace(ChatRequestId { document, latest: request_id, token: Some(token.clone()) })
            .and_then(|chat| chat.token) {
            previous.cancel();
        }
        Ok((self.epoch, selected.clone(), token))
    }

    fn finish(&mut self, document: u64, local: &LocalReview, targets: &[SelectedTarget], request_id: u32, epoch: u64, candidate: ReviewCandidate) -> Result<Proposal> {
        if self.epoch != epoch {
            return Err(STALE.into());
        }
        self.chat.as_mut().filter(|chat| chat.document == document && chat.latest == request_id)
            .and_then(|chat| chat.token.take()).ok_or(CANCELLED)?;
        let selected = self.selected(document, targets)?;
        let id = self.reviews.propose(Candidate::Chat(candidate), Some(selected), self.policy).map_err(review_error)?;
        let card = self.reviews.open(local, id, Some(selected), self.policy, Some(request_id)).map_err(review_error)?;
        project_card(id, card)
    }

    fn cancel(&mut self, document: u64, local: &LocalReview, request_id: u32) {
        if self.cancelled_through.is_none_or(|(owner, id)| owner != document || id < request_id) {
            self.cancelled_through = Some((document, request_id));
        }
        if let Some(token) = self.chat.as_mut().filter(|chat| chat.document == document && chat.latest == request_id)
            .and_then(|chat| chat.token.take()) {
            token.cancel();
        }
        self.reviews.cancel_chat(local, request_id);
    }

    fn open_cli(&mut self, document: u64, local: &LocalReview, targets: &[SelectedTarget]) -> Result<Option<Proposal>> {
        let selected = self.selected(document, targets)?;
        let Some(id) = self.reviews.cli_id(selected, self.policy) else { return Ok(None) };
        let card = self.reviews.open(local, id, Some(selected), self.policy, None).map_err(review_error)?;
        project_card(id, card).map(Some)
    }

    fn decide(&mut self, document: u64, local: &LocalReview, targets: &[SelectedTarget], request: Decision) -> Result<RecordedDecision> {
        let selected = self.selected(document, targets)?;
        let request = DecisionRequest {
            proposal_id: request.proposal_id.parse().map_err(|_| INVALID)?,
            choice: match request.decision {
                PanelChoice::Approve => Choice::Approve,
                PanelChoice::Reject => Choice::Reject,
                PanelChoice::Cancel => Choice::Cancel,
            },
        };
        self.reviews.decide(local, request, Some(selected), self.policy).map_err(review_error)
    }

    fn approve_execution(
        &mut self,
        local: &LocalReview,
        targets: &[SelectedTarget],
        request: &ExecApproveRequest,
        live: &execution::LiveSession,
        document: u64,
        exec: &execution::State,
        now: Instant,
    ) -> Result<execution::Card> {
        if request.session_id != live.id || request.selection_id != live.selection_id.to_string() {
            return Err(EXEC_SESSION.into());
        }
        let proposal_id = parse_proposal_id(&request.proposal_id)?;
        let task = execution::task(&request.task_id).ok_or_else(|| "지원하지 않는 작업입니다".to_string())?;
        let recorded = self.reviews.recorded(local, proposal_id).map_err(review_error)?;
        let target = targets.iter().find(|target| **target == recorded.target).ok_or_else(|| STALE.to_string())?;
        let card = self.reviews.approved_for(local, proposal_id, Some(target), self.policy).map_err(review_error)?;
        if card.operation != Operation::Uptime {
            return Err("승인된 조회 작업만 실행할 수 있습니다".into());
        }
        if !execution::matches_session(&card.target, &live.identity) {
            return Err(EXEC_SESSION.into());
        }
        exec.issue(execution::NewApproval {
            proposal_id,
            target: card.target.clone(),
            policy: self.policy,
            session: live.clone(),
            document,
            task,
        }, now).map_err(|error| error.to_string())
    }

    fn start_execution(
        &mut self,
        local: &LocalReview,
        targets: &[SelectedTarget],
        request: &ExecStartRequest,
        live: &execution::LiveSession,
        document: u64,
        exec: &execution::State,
        now: Instant,
    ) -> Result<(execution::Approval, Arc<execution::Run>, std::sync::mpsc::Receiver<execution::Command>)> {
        let approval_id = parse_exec_id(&request.approval_id)?;
        let generation = parse_exec_id(&request.generation)?;
        let snapshot = exec.snapshot(approval_id).ok_or_else(|| EXEC_APPROVAL.to_string())?;
        if snapshot.policy != self.policy {
            return Err(EXEC_POLICY.into());
        }
        if request.session_id != snapshot.session.id
            || generation != snapshot.session.selection_id
            || live.id != snapshot.session.id
            || live.selection_id != snapshot.session.selection_id
            || live.identity != snapshot.session.identity
        {
            return Err(EXEC_SESSION.into());
        }
        let target = targets.iter().find(|target| **target == snapshot.target).ok_or_else(|| STALE.to_string())?;
        let card = self.reviews.approved_for(local, snapshot.proposal_id, Some(target), self.policy).map_err(review_error)?;
        if card.operation != Operation::Uptime {
            return Err("승인된 조회 작업만 실행할 수 있습니다".into());
        }
        exec.start(
            &execution::Start {
                approval_id,
                task_id: request.task_id.clone(),
                session_id: request.session_id.clone(),
                generation,
            },
            live,
            self.policy,
            document,
            now,
        ).map_err(|error| error.to_string())
    }

    fn history(&self, local: &LocalReview) -> Result<Value> {
        let records = self.reviews.history(local).map(|(card, decision)| {
            Ok(HistoryEntry { proposal: project_card(card.proposal_id, card)?, decision })
        }).collect::<Result<Vec<_>>>()?;
        serde_json::to_value(records).map_err(|_| INVALID.into())
    }
}

#[cfg(test)]
#[path = "ipc/tests.rs"]
mod tests;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TargetRef {
    target_id: String,
    generation: String,
}

impl TargetRef {
    fn matches(&self, target: &SelectedTarget) -> bool {
        self.target_id == target.profile_id && self.generation.parse::<NonZeroU64>() == Ok(target.generation)
    }
}

impl From<&SelectedTarget> for TargetRef {
    fn from(target: &SelectedTarget) -> Self {
        Self { target_id: target.profile_id.clone(), generation: target.generation.to_string() }
    }
}

#[derive(Serialize)]
struct TargetOption {
    #[serde(flatten)]
    reference: TargetRef,
    label: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectRequest {
    target: Option<TargetRef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    role: Role,
    content: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role { User, Assistant }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProposalRequest {
    request_id: u32,
    target: TargetRef,
    model: String,
    operation: Uptime,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelProposalRequest { request_id: u32 }

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Uptime { Uptime }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Decision {
    proposal_id: String,
    decision: PanelChoice,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum PanelChoice { Approve, Reject, Cancel }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExecApproveRequest {
    proposal_id: String,
    task_id: String,
    session_id: String,
    selection_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExecStartRequest {
    session_id: String,
    generation: String,
    approval_id: String,
    task_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExecStopRequest {
    approval_id: String,
}

#[derive(Serialize)]
struct Proposal {
    id: String,
    target: TargetRef,
    node: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ssm: Option<SsmScope>,
    #[serde(flatten)]
    details: ProposalDetails,
}

#[derive(Serialize)]
struct HistoryEntry {
    proposal: Proposal,
    decision: RecordedDecision,
}

#[derive(Serialize)]
struct SsmScope {
    profile: String,
    region: String,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
enum ProposalDetails {
    Reviewable { display_command: &'static str, interpretation: &'static str },
    Blocked,
    Unregistered,
}

fn project_card(id: u32, card: &Card) -> Result<Proposal> {
    let details = match &card.operation {
        Operation::Uptime => ProposalDetails::Reviewable {
            display_command: card.command.ok_or(INVALID)?,
            interpretation: card.explanation,
        },
        Operation::Blocked { .. } => ProposalDetails::Blocked,
        Operation::RawReview { .. } => ProposalDetails::Unregistered,
    };
    Ok(Proposal {
        id: id.to_string(), target: (&card.target).into(), node: card.target.node_id.clone(),
        ssm: card.target.ssm_profile.as_ref().map(|profile| SsmScope {
            profile: profile.clone(), region: card.target.region.clone(),
        }),
        details,
    })
}

pub(crate) fn handles(command: &str) -> bool {
    matches!(command, "ai_targets" | "ai_models" | "ai_select" | "ai_chat" | "ai_propose" | "ai_cancel_proposal" | "ai_decide" | "ai_decisions" | "ai_cli_review" | "ai_policy" | "ai_set_policy"
        | "ai_exec_approve" | "ai_exec_start" | "ai_exec_status" | "ai_exec_stop")
}

pub(crate) fn dispatch(invoke: Invoke) {
    let caller = invoke.message.webview_ref();
    let document = caller.state::<Sessions>().document(caller.label());
    let Ok(local) = LocalReview::from_invoke(&invoke, document) else {
        invoke.resolver.reject("앱의 로컬 기본 창에서만 AI를 사용할 수 있습니다");
        return;
    };
    let Ok(request) = parse(invoke.message.command(), invoke.message.payload()) else {
        invoke.resolver.reject(INVALID);
        return;
    };
    let caller = caller.clone();
    invoke.resolver.respond_async(async move {
        handle(caller, document, local, request).await.map_err(InvokeError::from)
    });
}

enum Request {
    Models,
    Targets,
    Select(SelectRequest),
    Chat(ChatRequest),
    Propose(ProposalRequest),
    CancelProposal(CancelProposalRequest),
    Decide(Decision),
    Decisions,
    CliReview,
    Policy,
    SetPolicy(PolicyRequest),
    ExecApprove(ExecApproveRequest),
    ExecStart(ExecStartRequest),
    ExecStatus,
    ExecStop(ExecStopRequest),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyRequest { blocked: bool }

pub(super) async fn model_proposal(ollama: &Ollama, token: &CancellationToken, model: &str, target: SelectedTarget) -> Result<ReviewCandidate> {
    token.run_until_cancelled(ollama.propose(model, UPTIME, target))
        .await.ok_or(CANCELLED)?.map_err(|error| error.to_string())
}

fn parse(command: &str, body: &InvokeBody) -> Result<Request> {
    let InvokeBody::Json(value) = body else { return Err(INVALID.into()) };
    match command {
        "ai_models" | "ai_targets" if value.as_object().is_some_and(|fields| fields.is_empty()) => {
            Ok(if command == "ai_models" { Request::Models } else { Request::Targets })
        }
        "ai_cli_review" if value.as_object().is_some_and(|fields| fields.is_empty()) => return Ok(Request::CliReview),
        "ai_select" => SelectRequest::deserialize(value).map(Request::Select),
        "ai_chat" => ChatRequest::deserialize(value).map(Request::Chat),
        "ai_propose" => ProposalRequest::deserialize(value).map(Request::Propose),
        "ai_cancel_proposal" => CancelProposalRequest::deserialize(value).map(Request::CancelProposal),
        "ai_decide" => Decision::deserialize(value).map(Request::Decide),
        "ai_decisions" if value.as_object().is_some_and(|fields| fields.is_empty()) => return Ok(Request::Decisions),
        "ai_policy" if value.as_object().is_some_and(|fields| fields.is_empty()) => return Ok(Request::Policy),
        "ai_set_policy" => PolicyRequest::deserialize(value).map(Request::SetPolicy),
        "ai_exec_approve" => ExecApproveRequest::deserialize(value).map(Request::ExecApprove),
        "ai_exec_start" => ExecStartRequest::deserialize(value).map(Request::ExecStart),
        "ai_exec_status" if value.as_object().is_some_and(|fields| fields.is_empty()) => return Ok(Request::ExecStatus),
        "ai_exec_stop" => ExecStopRequest::deserialize(value).map(Request::ExecStop),
        _ => return Err(INVALID.into()),
    }.map_err(|_| INVALID.into())
}

fn confirmed_session(caller: &Webview, document: u64) -> Result<(NonZeroU64, crate::terminal::LiveSsmIdentity)> {
    let sessions = caller.state::<Sessions>();
    caller.state::<session_selection::State>().confirmed(&sessions, document)
        .ok_or_else(|| "AI 작업 세션을 선택하고 연결을 확인하세요".to_string())
}

fn parse_proposal_id(value: &str) -> Result<u32> {
    value.parse().map_err(|_| INVALID.into())
}

fn parse_exec_id(value: &str) -> Result<NonZeroU64> {
    if value.is_empty() || value.starts_with('0') || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(INVALID.into());
    }
    value.parse().map_err(|_| INVALID.into())
}

fn document_check<T>(caller: &Webview, document: u64, work: impl FnOnce() -> Result<T>) -> Result<T> {
    caller.state::<Sessions>().with_probes(caller.label(), document, |_| work())
}

async fn stored<T: Send + 'static>(
    caller: Webview,
    document: u64,
    work: impl FnOnce(&mut ReviewState, Vec<SelectedTarget>) -> Result<T> + Send + 'static,
) -> Result<T> {
    tauri::async_runtime::spawn_blocking(move || {
        let directory = caller.path().app_config_dir().map_err(|_| "앱 설정 디렉터리를 찾을 수 없습니다")?;
        connections::with_ai_targets(&directory, |targets| {
            document_check(&caller, document, || work(&mut caller.state::<State>().0.lock().unwrap(), targets))
        })?
    }).await.map_err(|_| "조회 대상 작업에 실패했습니다")?
}

async fn handle(caller: Webview, document: u64, local: LocalReview, request: Request) -> Result<Value> {
    document_check(&caller, document, || Ok(()))?;
    match request {
        Request::Models => {
            let models = Ollama::new().map_err(|error| error.to_string())?.models().await.map_err(|error| error.to_string())?;
            document_check(&caller, document, || Ok(json!(models)))
        }
        Request::Targets => stored(caller, document, |_, targets| {
            let targets: Vec<_> = targets.into_iter().map(|target| TargetOption {
                reference: (&target).into(), label: target.display_name,
            }).collect();
            Ok(json!(targets))
        }).await,
        Request::Select(request) => stored(caller, document, move |state, targets| {
            let target = request.target.and_then(|reference| targets.into_iter().find(|target| reference.matches(target)));
            let valid = target.is_some();
            state.select(document, target)?;
            Ok(json!(valid))
        }).await,
        Request::Chat(request) => {
            let messages: Vec<_> = request.messages.iter().map(|message| ChatMessage {
                role: match message.role { Role::User => ChatRole::User, Role::Assistant => ChatRole::Assistant },
                content: &message.content,
            }).collect();
            let result = Ollama::new().map_err(|error| error.to_string())?.chat(&request.model, &messages).await;
            document_check(&caller, document, || Ok(match result {
                Ok(content) => json!({"kind": "reply", "content": content}),
                Err(error) => json!({"kind": "error", "message": error.to_string()}),
            }))
        }
        Request::Propose(request) => {
            let ProposalRequest { request_id, target, model, operation: Uptime::Uptime } = request;
            let (epoch, selected, token) = stored(caller.clone(), document, move |state, targets| state.begin(document, &targets, request_id, target)).await?;
            let candidate = model_proposal(&Ollama::new().map_err(|error| error.to_string())?, &token, &model, selected).await?;
            stored(caller, document, move |state, targets| {
                let proposal = state.finish(document, &local, &targets, request_id, epoch, candidate)?;
                serde_json::to_value(proposal).map_err(|_| INVALID.into())
            }).await
        }
        Request::CancelProposal(request) => document_check(&caller, document, || {
            caller.state::<State>().0.lock().unwrap().cancel(document, &local, request.request_id);
            Ok(Value::Null)
        }),
        Request::Decide(request) => stored(caller, document, move |state, targets| {
            Ok(json!(state.decide(document, &local, &targets, request)?))
        }).await,
        Request::Decisions => document_check(&caller, document, || {
            caller.state::<State>().0.lock().unwrap().history(&local)
        }),
        Request::CliReview => stored(caller, document, move |state, targets| {
            let card = state.open_cli(document, &local, &targets)?;
            serde_json::to_value(card).map_err(|_| INVALID.into())
        }).await,
        Request::Policy => document_check(&caller, document, || {
            Ok(json!(caller.state::<State>().0.lock().unwrap().policy.blocked))
        }),
        Request::SetPolicy(request) => document_check(&caller, document, || {
            caller.state::<State>().0.lock().unwrap().set_policy(request.blocked)?;
            Ok(Value::Null)
        }),
        Request::ExecApprove(request) => {
            let (selection_id, identity) = confirmed_session(&caller, document)?;
            let live = execution::LiveSession { id: identity.id.clone(), selection_id, identity };
            let exec = caller.state::<execution::State>().inner().clone();
            let now = Instant::now();
            stored(caller, document, move |state, targets| {
                let card = state.approve_execution(&local, &targets, &request, &live, document, &exec, now)?;
                serde_json::to_value(card).map_err(|_| INVALID.into())
            }).await
        }
        Request::ExecStart(request) => {
            let (selection_id, identity) = confirmed_session(&caller, document)?;
            let live = execution::LiveSession { id: identity.id.clone(), selection_id, identity };
            let exec = caller.state::<execution::State>().inner().clone();
            let started = exec.clone();
            let now = Instant::now();
            let (approval, run, commands) = stored(caller.clone(), document, move |state, targets| {
                state.start_execution(&local, &targets, &request, &live, document, &started, now)
            }).await?;
            execution::launch(caller.app_handle().clone(), run, approval, commands);
            let view = exec.status(document).ok_or_else(|| "실행 상태를 읽을 수 없습니다".to_string())?;
            serde_json::to_value(view).map_err(|_| INVALID.into())
        }
        Request::ExecStatus => document_check(&caller, document, || {
            match caller.state::<execution::State>().status(document) {
                Some(view) => serde_json::to_value(view).map_err(|_| INVALID.into()),
                None => Ok(Value::Null),
            }
        }),
        Request::ExecStop(request) => {
            let approval_id = parse_exec_id(&request.approval_id)?;
            document_check(&caller, document, || {
                let view = caller.state::<execution::State>().stop(approval_id, document)
                    .map_err(|error| error.to_string())?;
                serde_json::to_value(view).map_err(|_| INVALID.into())
            })
        }
    }
}

fn review_error(error: super::approval::ReviewError) -> String {
    use super::approval::ReviewError::{Expired, Missing, Stale, TargetMismatch, TargetUnavailable};
    match error {
        Stale | TargetMismatch | TargetUnavailable => STALE,
        Expired | Missing => "제안 검토가 종료되었습니다",
        _ => "제안 결정을 기록할 수 없습니다",
    }.into()
}
