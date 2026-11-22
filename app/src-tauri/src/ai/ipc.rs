use std::{num::NonZeroU64, sync::Mutex, time::{Duration, Instant}};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{Manager, Webview, ipc::{Invoke, InvokeBody, InvokeError}};

use crate::{connections, terminal::Sessions};
use super::{ChatMessage, ChatRole, Ollama, Operation, SelectedTarget, UPTIME,
    approval::{Candidate, Card, Choice, Decision as RecordedDecision, DecisionRequest, LocalReview, Policy, Reviews}};
use super::ReviewCandidate;
use super::cli::{Proposal as CliProposal, Query, Rejected};
use crate::connections::profile::ValidatedDraft;

const DRAFT_TIMEOUT: Duration = Duration::from_secs(300);
const STALE: &str = "조회 대상 또는 문서가 바뀌었습니다";
const INVALID: &str = "AI 요청 형식이 올바르지 않습니다";
const CANCELLED: &str = "제안 요청이 취소되었습니다";
type Result<T, E = String> = std::result::Result<T, E>;

#[derive(Default)]
pub(crate) struct State(Mutex<ReviewState>);

impl State {
    pub(super) fn receive_draft(&self, draft: ValidatedDraft, deadline: Instant) -> Result<String, Rejected> {
        let mut state = self.0.lock().unwrap();
        let now = Instant::now();
        if now >= deadline { return Err(Rejected); }
        if let Some(pending) = state.draft.as_ref().filter(|pending| now < pending.expires_at) {
            if pending.initial_draft.matches(&draft) {
                return Ok(pending.draft_id.clone());
            }
            if !pending.opened { return Err(Rejected); }
        }
        state.next_draft = state.next_draft.checked_add(1).ok_or(Rejected)?;
        let draft_id = format!("cli-{}", state.next_draft);
        state.draft = Some(ReceivedDraft {
            draft_id: draft_id.clone(), initial_draft: draft, expires_at: now + DRAFT_TIMEOUT, opened: false,
        });
        Ok(draft_id)
    }

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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReceivedDraft {
    draft_id: String,
    initial_draft: ValidatedDraft,
    #[serde(skip)]
    expires_at: Instant,
    #[serde(skip)]
    opened: bool,
}

#[derive(Default)]
struct ReviewState {
    selection: Option<Selection>,
    epoch: u64,
    chat: Option<ChatRequestId>,
    reviews: Reviews,
    policy: Policy,
    next_draft: u64,
    draft: Option<ReceivedDraft>,
}

struct Selection {
    document: u64,
    target: SelectedTarget,
}

// 문서 안에서 증가하는 ID로 시작보다 먼저 도착한 취소도 보존한다.
struct ChatRequestId {
    document: u64,
    latest: u32,
    active: bool,
}

impl ReviewState {
    fn set_policy(&mut self, blocked: bool) -> Result<()> {
        if self.policy.blocked != blocked {
            let revision = self.policy.revision.checked_add(1).ok_or(STALE)?;
            self.epoch = self.epoch.checked_add(1).ok_or(STALE)?;
            self.policy = Policy { revision, blocked };
            self.reviews.clear_decisions();
        }
        Ok(())
    }

    fn open_draft(&mut self) -> Result<Value> {
        let Some(draft) = self.draft.as_mut().filter(|draft| !draft.opened && Instant::now() < draft.expires_at) else {
            return Ok(Value::Null);
        };
        let value = serde_json::to_value(&*draft).map_err(|_| INVALID)?;
        draft.opened = true;
        Ok(value)
    }

    fn select(&mut self, document: u64, target: Option<SelectedTarget>) -> Result<()> {
        self.epoch = self.epoch.checked_add(1).ok_or(STALE)?;
        self.reviews.clear();
        self.selection = target.map(|target| Selection { document, target });
        Ok(())
    }

    fn selected<'a>(&self, document: u64, targets: &'a [SelectedTarget]) -> Result<&'a SelectedTarget> {
        let selection = self.selection.as_ref().filter(|value| value.document == document).ok_or(STALE)?;
        targets.iter().find(|target| *target == &selection.target).ok_or_else(|| STALE.into())
    }

    fn begin(&mut self, document: u64, targets: &[SelectedTarget], request_id: u32, target: TargetRef) -> Result<(u64, SelectedTarget)> {
        let selected = self.selected(document, targets)?;
        if !target.matches(selected) {
            return Err(STALE.into());
        }
        if self.chat.as_ref().is_some_and(|chat| chat.document == document && request_id <= chat.latest) {
            return Err(CANCELLED.into());
        }
        self.chat = Some(ChatRequestId { document, latest: request_id, active: true });
        Ok((self.epoch, selected.clone()))
    }

    fn finish(&mut self, document: u64, local: &LocalReview, targets: &[SelectedTarget], request_id: u32, epoch: u64, candidate: ReviewCandidate) -> Result<Proposal> {
        if self.epoch != epoch {
            return Err(STALE.into());
        }
        let chat = self.chat.as_mut().filter(|chat| chat.document == document && chat.latest == request_id && chat.active)
            .ok_or(CANCELLED)?;
        chat.active = false;
        let selected = self.selected(document, targets)?;
        let id = self.reviews.propose(Candidate::Chat(candidate), Some(selected), self.policy).map_err(review_error)?;
        let card = self.reviews.open(local, id, Some(selected), self.policy, Some(request_id)).map_err(review_error)?;
        project_card(id, card)
    }

    fn cancel(&mut self, document: u64, local: &LocalReview, request_id: u32) {
        if !self.chat.as_ref().is_some_and(|chat| chat.document == document && request_id < chat.latest) {
            self.chat = Some(ChatRequestId { document, latest: request_id, active: false });
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
    matches!(command, "ai_targets" | "ai_models" | "ai_select" | "ai_chat" | "ai_propose" | "ai_cancel_proposal" | "ai_decide" | "ai_cli_review" | "ai_cli_draft" | "ai_policy" | "ai_set_policy")
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
    CliReview,
    CliDraft,
    Policy,
    SetPolicy(PolicyRequest),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyRequest { blocked: bool }

fn parse(command: &str, body: &InvokeBody) -> Result<Request> {
    let InvokeBody::Json(value) = body else { return Err(INVALID.into()) };
    match command {
        "ai_models" | "ai_targets" if value.as_object().is_some_and(|fields| fields.is_empty()) => {
            Ok(if command == "ai_models" { Request::Models } else { Request::Targets })
        }
        "ai_cli_review" | "ai_cli_draft" if value.as_object().is_some_and(|fields| fields.is_empty()) => {
            Ok(if command == "ai_cli_review" { Request::CliReview } else { Request::CliDraft })
        }
        "ai_select" => SelectRequest::deserialize(value).map(Request::Select),
        "ai_chat" => ChatRequest::deserialize(value).map(Request::Chat),
        "ai_propose" => ProposalRequest::deserialize(value).map(Request::Propose),
        "ai_cancel_proposal" => CancelProposalRequest::deserialize(value).map(Request::CancelProposal),
        "ai_decide" => Decision::deserialize(value).map(Request::Decide),
        "ai_policy" if value.as_object().is_some_and(|fields| fields.is_empty()) => return Ok(Request::Policy),
        "ai_set_policy" => PolicyRequest::deserialize(value).map(Request::SetPolicy),
        _ => return Err(INVALID.into()),
    }.map_err(|_| INVALID.into())
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
            let (epoch, selected) = stored(caller.clone(), document, move |state, targets| state.begin(document, &targets, request_id, target)).await?;
            let candidate = Ollama::new().map_err(|error| error.to_string())?
                .propose(&model, UPTIME, selected).await.map_err(|error| error.to_string())?;
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
        Request::CliReview => stored(caller, document, move |state, targets| {
            let card = state.open_cli(document, &local, &targets)?;
            serde_json::to_value(card).map_err(|_| INVALID.into())
        }).await,
        Request::CliDraft => document_check(&caller, document, || {
            caller.state::<State>().0.lock().unwrap().open_draft()
        }),
        Request::Policy => document_check(&caller, document, || {
            Ok(json!(caller.state::<State>().0.lock().unwrap().policy.blocked))
        }),
        Request::SetPolicy(request) => document_check(&caller, document, || {
            caller.state::<State>().0.lock().unwrap().set_policy(request.blocked)?;
            Ok(Value::Null)
        }),
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
