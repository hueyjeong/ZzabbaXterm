use super::{ReviewCandidate, SelectedTarget, UPTIME, review_proposal};
use reqwest::{Client, StatusCode, redirect, retry};
use serde::{Deserialize, Serialize, de::IgnoredAny};
use serde_json::{Value, json};
use std::time::Duration;
use thiserror::Error;

const CHAT_URL: &str = "http://127.0.0.1:11434/api/chat";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_MODEL_BYTES: usize = 256;
const MAX_MESSAGES: usize = 256;
const MAX_MESSAGE_BYTES: usize = 32 * 1024;
const MAX_HISTORY_BYTES: usize = 128 * 1024;
const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

const PROPOSAL_INSTRUCTIONS: &str = "Return only the schema object. Select uptime only for a single \
    request to read uptime or load from the selected node. For other, combined, or ambiguous requests, \
    select unsupported. Do not supply arguments, target, approval, or commands.";

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    User,
    Assistant,
    System,
}

#[derive(Serialize)]
pub struct ChatMessage<'a> {
    pub role: ChatRole,
    pub content: &'a str,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage<'a>],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<Value>,
}

#[derive(Deserialize)]
struct ChatResponse {
    done: bool,
    message: AssistantMessage,
}

#[derive(Deserialize)]
struct AssistantMessage {
    role: ChatRole,
    content: String,
    tool_calls: Option<Vec<IgnoredAny>>,
}

#[derive(Debug, Error)]
pub enum OllamaError {
    #[error("설치된 로컬 Ollama 모델을 선택하세요.")]
    InvalidModel,
    #[error("선택한 로컬 Ollama 모델이 설치되어 있지 않습니다.")]
    ModelNotInstalled,
    #[error("Ollama 연결에 실패했습니다.")]
    Connection,
    #[error("Ollama 응답 대기 시간이 초과됐습니다.")]
    Timeout,
    #[error("Ollama HTTP 오류: {0}")]
    Http(StatusCode),
    #[error("Ollama 요청 크기 한도를 초과했습니다. 전송되지 않았습니다.")]
    RequestTooLarge,
    #[error("Ollama 요청 형식이 올바르지 않습니다.")]
    InvalidRequest,
    #[error("Ollama 응답 크기 한도를 초과했습니다.")]
    ResponseTooLarge,
    #[error("Ollama 응답 형식이 올바르지 않습니다.")]
    InvalidResponse,
    #[error("제안 형식이 올바르지 않습니다.")]
    InvalidProposal,
    #[error("지원하는 조회 제안이 없습니다.")]
    UnsupportedProposal,
}

/// `:local` 모델 선택을 지원하는 Ollama 서버에 연결합니다.
pub struct Ollama {
    client: Client,
    #[cfg(test)]
    test_url: Option<String>,
}

impl Ollama {
    pub fn new() -> Result<Self, OllamaError> {
        Self::with_timeout(REQUEST_TIMEOUT)
    }

    fn with_timeout(timeout: Duration) -> Result<Self, OllamaError> {
        let client = Client::builder()
            .no_proxy()
            .redirect(redirect::Policy::none())
            .retry(retry::never())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(Duration::from_secs(3))
            .timeout(timeout)
            .build()
            .map_err(transport_error)?;
        Ok(Self {
            client,
            #[cfg(test)]
            test_url: None,
        })
    }

    pub async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage<'_>],
    ) -> Result<String, OllamaError> {
        Ok(self.send(model, messages, None).await?.content)
    }

    /// 현재 지원하는 입력은 `가동 시간 조회`, `가동 시간을 알려 줘`,
    /// `Show uptime`, `uptime`입니다. 반환값은 선택한 대상의 검토 후보입니다.
    pub async fn propose(
        &self,
        model: &str,
        request: &str,
        target: SelectedTarget,
    ) -> Result<ReviewCandidate, OllamaError> {
        if request.len() > MAX_MESSAGE_BYTES {
            return Err(OllamaError::RequestTooLarge);
        }
        // shortcut: 정확히 등록한 조회 문구만 받으며, 새 조회를 추가할 때 등록 범위를 넓힙니다.
        if !matches!(
            request,
            "가동 시간 조회" | "가동 시간을 알려 줘" | "Show uptime" | UPTIME
        ) {
            return Err(OllamaError::UnsupportedProposal);
        }
        let messages = [
            ChatMessage {
                role: ChatRole::System,
                content: PROPOSAL_INSTRUCTIONS,
            },
            ChatMessage {
                role: ChatRole::User,
                content: request,
            },
        ];
        let format = json!({
            "type": "object",
            "properties": {
                "operation": { "type": "string", "enum": [UPTIME, "unsupported"] },
                "arguments": { "type": "object", "additionalProperties": false },
            },
            "required": ["operation", "arguments"],
            "additionalProperties": false,
        });
        let message = self.send(model, &messages, Some(format)).await?;
        if message.tool_calls.is_some_and(|calls| !calls.is_empty()) {
            return Err(OllamaError::InvalidProposal);
        }
        review_proposal(&message.content, target)
    }

    async fn send(
        &self,
        model: &str,
        messages: &[ChatMessage<'_>],
        format: Option<Value>,
    ) -> Result<AssistantMessage, OllamaError> {
        if model.len() > MAX_MODEL_BYTES || messages.len() > MAX_MESSAGES {
            return Err(OllamaError::RequestTooLarge);
        }
        let mut history_bytes = 0;
        for message in messages {
            let bytes = message.content.len();
            if bytes > MAX_MESSAGE_BYTES || bytes > MAX_HISTORY_BYTES - history_bytes {
                return Err(OllamaError::RequestTooLarge);
            }
            history_bytes += bytes;
        }
        let model = local_model_ref(model)?;
        let request_body = serde_json::to_vec(&ChatRequest {
            model: &model,
            messages,
            stream: false,
            format,
        })
        .map_err(|_| OllamaError::InvalidRequest)?;
        // JSON 이스케이프와 구조화 스키마가 포함된 실제 본문을 제한합니다.
        if request_body.len() > MAX_REQUEST_BYTES {
            return Err(OllamaError::RequestTooLarge);
        }
        let url = CHAT_URL;
        #[cfg(test)]
        let url = self.test_url.as_deref().unwrap_or(url);
        let mut response = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(request_body)
            .send()
            .await
            .map_err(transport_error)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(OllamaError::ModelNotInstalled);
        }
        if !response.status().is_success() {
            return Err(OllamaError::Http(response.status()));
        }
        let length = response.content_length().unwrap_or(0);
        if length > MAX_RESPONSE_BYTES as u64 {
            return Err(OllamaError::ResponseTooLarge);
        }
        let mut body = Vec::with_capacity(length as usize);
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
                return Err(OllamaError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        let response: ChatResponse =
            serde_json::from_slice(&body).map_err(|_| OllamaError::InvalidResponse)?;
        if !response.done || !matches!(response.message.role, ChatRole::Assistant) {
            return Err(OllamaError::InvalidResponse);
        }
        Ok(response.message)
    }
}

fn local_model_ref(model: &str) -> Result<String, OllamaError> {
    let (name, tag) = model.split_once(':').unwrap_or((model, "latest"));
    let valid_part = |part: &str| {
        part.starts_with(|c: char| c.is_ascii_alphanumeric())
            && part
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    };
    if !name.split('/').all(valid_part)
        || !valid_part(tag)
        || tag.eq_ignore_ascii_case("cloud")
        || tag.eq_ignore_ascii_case("local")
        || tag
            .rsplit_once('-')
            .is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("cloud"))
    {
        return Err(OllamaError::InvalidModel);
    }
    // Ollama에서 로컬 이름을 가진 클라우드 모델도 추론 전에 거부하게 합니다.
    Ok(format!("{model}:local"))
}

fn transport_error(error: reqwest::Error) -> OllamaError {
    if error.is_timeout() {
        OllamaError::Timeout
    } else {
        OllamaError::Connection
    }
}

#[cfg(test)]
mod tests;
