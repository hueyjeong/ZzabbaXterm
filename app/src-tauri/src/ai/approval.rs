use std::{
    collections::{HashMap, hash_map::Entry},
    num::NonZeroU64,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tauri::ipc::Invoke;

use super::{Operation, ReviewCandidate, SelectedTarget, UPTIME};

const REVIEW_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum Source {
    Chat,
    Cli,
}

pub enum Candidate<'a> {
    Chat(ReviewCandidate),
    Cli {
        target_id: &'a str,
        operation: &'a str,
    },
}

// 앱 상태에서 다시 읽는다. 규칙이 바뀔 때마다 개정 번호를 증가시킨다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub revision: NonZeroU64,
    pub blocked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum Mode {
    Review,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Card {
    proposal_id: u32,
    source: Source,
    pub(super) target: SelectedTarget,
    pub(super) operation: Operation,
    mode: Mode,
    pub(super) command: Option<&'static str>,
    pub(super) explanation: &'static str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecisionRequest {
    pub(super) proposal_id: u32,
    pub(super) choice: Choice,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum Choice {
    Approve,
    Reject,
    Cancel,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Decision {
    Approved,
    Rejected,
    Cancelled,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRecord {
    card: Card,
    decision: Decision,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewError {
    NotLocalMain,
    TargetUnavailable,
    TargetMismatch,
    Missing,
    Stale,
    Expired,
    NotReviewed,
    NotApprovable,
    IdExhausted,
}

pub struct LocalReview(u64);

impl LocalReview {
    // 호출에 붙은 ACL과 문서 세대는 Rust에서 읽으며, 요청 JSON의 값을 받지 않는다.
    pub fn from_invoke(invoke: &Invoke, document: u64) -> Result<Self, ReviewError> {
        let webview = invoke.message.webview_ref();
        Self::check(
            webview.label(),
            webview.window_ref().label(),
            invoke.acl.is_some(),
            document,
        )
    }

    pub(super) fn check(
        webview: &str,
        window: &str,
        has_acl: bool,
        document: u64,
    ) -> Result<Self, ReviewError> {
        if webview == "main" && window == "main" && has_acl {
            Ok(Self(document))
        } else {
            Err(ReviewError::NotLocalMain)
        }
    }
}

struct Pending {
    card: Card,
    policy: Policy,
    expires_at: Instant,
    opened_in: Option<u64>,
}

impl Pending {
    fn check(
        &self,
        local: &LocalReview,
        selected: Option<&SelectedTarget>,
        policy: Policy,
    ) -> Result<(), ReviewError> {
        if Instant::now() >= self.expires_at {
            Err(ReviewError::Expired)
        } else if selected != Some(&self.card.target)
            || policy != self.policy
            || self.opened_in.is_some_and(|document| document != local.0)
        {
            Err(ReviewError::Stale)
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
pub struct Reviews {
    next_id: u32,
    pending: HashMap<u32, Pending>,
}

impl Reviews {
    pub(super) fn clear(&mut self) {
        self.pending.clear();
    }

    pub fn propose(
        &mut self,
        candidate: Candidate<'_>,
        selected: Option<&SelectedTarget>,
        policy: Policy,
    ) -> Result<u32, ReviewError> {
        let target = selected.ok_or(ReviewError::TargetUnavailable)?;
        let (source, text, operation, target) = match candidate {
            Candidate::Chat(candidate) if &candidate.target == target => (
                Source::Chat,
                UPTIME,
                Some(candidate.operation),
                candidate.target,
            ),
            Candidate::Chat(_) => return Err(ReviewError::Stale),
            Candidate::Cli {
                target_id,
                operation,
            } if target_id == target.profile_id => (
                Source::Cli,
                operation,
                (operation == UPTIME).then_some(Operation::Uptime),
                target.clone(),
            ),
            Candidate::Cli { .. } => return Err(ReviewError::TargetMismatch),
        };
        let (operation, command, explanation) = if policy.blocked {
            (
                Operation::Blocked {
                    text: escaped(text),
                },
                None,
                "정책에서 차단한 제안입니다",
            )
        } else if let Some(operation) = operation {
            (
                operation,
                Some(UPTIME),
                "선택한 노드의 가동 시간과 부하를 조회합니다",
            )
        } else {
            (
                Operation::RawReview {
                    text: escaped(text),
                },
                None,
                "원문만 검토할 수 있습니다",
            )
        };
        let now = Instant::now();
        self.pending.retain(|_, proposal| proposal.expires_at > now);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(ReviewError::IdExhausted)?;
        let proposal_id = self.next_id;
        let card = Card {
            proposal_id,
            source,
            target,
            operation,
            mode: Mode::Review,
            command,
            explanation,
        };
        self.pending.insert(
            proposal_id,
            Pending {
                card,
                policy,
                expires_at: now + REVIEW_TIMEOUT,
                opened_in: None,
            },
        );
        Ok(proposal_id)
    }

    pub fn open(
        &mut self,
        local: &LocalReview,
        proposal_id: u32,
        selected: Option<&SelectedTarget>,
        policy: Policy,
    ) -> Result<&Card, ReviewError> {
        let Entry::Occupied(entry) = self.pending.entry(proposal_id) else {
            return Err(ReviewError::Missing);
        };
        if let Err(error) = entry.get().check(local, selected, policy) {
            entry.remove();
            return Err(error);
        }
        let pending = entry.into_mut();
        pending.opened_in = Some(local.0);
        Ok(&pending.card)
    }

    pub fn decide(
        &mut self,
        local: &LocalReview,
        request: DecisionRequest,
        selected: Option<&SelectedTarget>,
        policy: Policy,
    ) -> Result<ReviewRecord, ReviewError> {
        let pending = self
            .pending
            .remove(&request.proposal_id)
            .ok_or(ReviewError::Missing)?;
        pending.check(local, selected, policy)?;
        if pending.opened_in.is_none() {
            return Err(ReviewError::NotReviewed);
        }
        let decision = match request.choice {
            Choice::Approve if pending.card.operation == Operation::Uptime => Decision::Approved,
            Choice::Approve => return Err(ReviewError::NotApprovable),
            Choice::Reject => Decision::Rejected,
            Choice::Cancel => Decision::Cancelled,
        };
        Ok(ReviewRecord {
            card: pending.card,
            decision,
        })
    }
}

fn escaped(text: &str) -> String {
    let mut display = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_control()
            || matches!(ch, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            display.extend(ch.escape_default());
        } else if ch == '\\' {
            display.push_str("\\\\");
        } else {
            display.push(ch);
        }
    }
    display
}

#[cfg(test)]
#[path = "approval/tests.rs"]
mod tests;
