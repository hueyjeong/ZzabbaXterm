pub mod approval;
pub(crate) mod ipc;
mod ollama;

pub use ollama::{ChatMessage, ChatRole, Ollama, OllamaError};

use serde::{Deserialize, Serialize, de::IgnoredAny};
use std::{borrow::Cow, collections::BTreeMap, num::NonZeroU64};

/// 앱에서 현재 UI 선택을 실제 연결로 해석한 대상입니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedTarget {
    pub profile_id: String,
    pub region: String,
    pub node_id: String,
    pub display_name: String,
    #[serde(skip)]
    pub generation: NonZeroU64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReviewCandidate {
    target: SelectedTarget,
    operation: Operation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum Operation {
    Uptime,
    RawReview { text: String },
    Blocked { text: String },
}

const UPTIME: &str = "uptime";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal<'a> {
    #[serde(borrow)]
    operation: Cow<'a, str>,
    arguments: BTreeMap<String, IgnoredAny>,
}

fn review_proposal(content: &str, target: SelectedTarget) -> Result<ReviewCandidate, OllamaError> {
    if !content.trim_start().starts_with('{') {
        return Err(OllamaError::InvalidProposal);
    }
    let proposal =
        serde_json::from_str::<Proposal>(content).map_err(|_| OllamaError::InvalidProposal)?;
    if !proposal.arguments.is_empty() {
        return Err(OllamaError::InvalidProposal);
    }
    let operation = match proposal.operation.as_ref() {
        UPTIME => Operation::Uptime,
        "unsupported" => return Err(OllamaError::UnsupportedProposal),
        _ => return Err(OllamaError::InvalidProposal),
    };
    Ok(ReviewCandidate { target, operation })
}
