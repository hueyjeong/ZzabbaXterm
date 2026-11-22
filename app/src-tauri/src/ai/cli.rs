use std::{
    fmt,
    io::{self, Read, Write},
    sync::Arc,
    num::NonZeroU64,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::Semaphore,
    time::{Instant, timeout, timeout_at},
};

use crate::{
    connections::{self, profile::{Draft, ValidatedDraft, valid_id}},
    local_ipc::{self, DEADLINE, Endpoint, Listener, MAX_FRAME, SERVICE, encode_frame, read_frame, write_frame},
};
use tauri::{AppHandle, Manager};

use super::ipc::State;

const MAX_STDIN: usize = MAX_FRAME - 64;
const USAGE: &str = "사용법: zzabba list | zzabba draft | zzabba propose";

type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Error {
    Usage,
    InvalidRequest,
    Unavailable,
    InvalidResponse,
    Rejected,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Usage => USAGE,
            Self::InvalidRequest => "잘못된 요청입니다.",
            Self::Unavailable => "ZzabbaXterm 앱에 연결할 수 없습니다.",
            Self::InvalidResponse => "ZzabbaXterm 앱의 응답이 올바르지 않습니다.",
            Self::Rejected => "ZzabbaXterm 앱이 요청을 거부했습니다.",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Query {
    Uptime,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Proposal {
    pub(crate) target_id: String,
    pub(crate) generation: String,
    pub(crate) operation: Query,
}

impl Proposal {
    fn validate(&self) -> Result<()> {
        if !valid_id(&self.target_id) || self.generation.parse::<NonZeroU64>().is_err() {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "camelCase", deny_unknown_fields)]
enum Request {
    List {},
    Draft { draft: Draft },
    Propose { proposal: Proposal },
}

#[derive(Serialize)]
#[serde(tag = "method", rename_all = "camelCase")]
enum Outbound {
    List,
    Draft { draft: ValidatedDraft },
    Propose { proposal: Proposal },
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Alias {
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) method: String,
    pub(crate) generation: String,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
enum Receipt {
    Listed {
        connections: Vec<Alias>,
    },
    Drafted {
        #[serde(rename = "draftId")]
        draft_id: String,
    },
    Proposed {
        #[serde(rename = "proposalId")]
        proposal_id: String,
    },
    Failed {
        error: Error,
    },
}

// 콜백은 조회와 초안·검토 요청 등록만 담당한다. 결정이나 실행 권한은 이 경계 밖에 있다.
pub(crate) trait Requests {
    fn list(&self) -> Result<Vec<Alias>, Rejected>;
    fn open_draft(&self, draft: ValidatedDraft, deadline: Instant) -> Result<String, Rejected>;
    fn request_review(&self, proposal: Proposal, deadline: Instant) -> Result<String, Rejected>;
}

#[derive(Debug)]
pub(crate) struct Rejected;

struct AppRequests(AppHandle);

impl Requests for AppRequests {
    fn list(&self) -> Result<Vec<Alias>, Rejected> {
        let directory = self.0.path().app_config_dir().map_err(|_| Rejected)?;
        connections::ai_aliases(&directory).map_err(|_| Rejected)
    }

    fn open_draft(&self, draft: ValidatedDraft, deadline: Instant) -> Result<String, Rejected> {
        self.0.state::<State>().receive_draft(draft, deadline.into_std())
    }

    fn request_review(&self, proposal: Proposal, deadline: Instant) -> Result<String, Rejected> {
        let directory = self.0.path().app_config_dir().map_err(|_| Rejected)?;
        connections::with_ai_targets(&directory, |targets| {
            self.0.state::<State>().receive_proposal(&targets, proposal, deadline.into_std())
        }).map_err(|_| Rejected)?
    }
}

pub(crate) fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let result = match bind() {
            Ok(listener) => serve(listener, AppRequests(app)).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            eprintln!("로컬 CLI 연결: {error}");
        }
    });
}

pub(crate) fn bind() -> io::Result<Listener> {
    Listener::bind(&Endpoint::for_service(SERVICE)?)
}

pub(crate) async fn serve(listener: Listener, requests: impl Requests + Send + Sync + 'static) -> io::Result<()> {
    let requests = Arc::new(requests);
    let blocking = Arc::new(Semaphore::new(4));
    local_ipc::serve(listener, move |mut stream| {
        let requests = Arc::clone(&requests);
        let blocking = Arc::clone(&blocking);
        async move {
            let _ = serve_connection(&mut stream, requests, blocking).await;
        }
    }).await
}

async fn serve_connection(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    requests: Arc<impl Requests + Send + Sync + 'static>,
    blocking: Arc<Semaphore>,
) -> io::Result<()> {
    let deadline = Instant::now() + DEADLINE;
    let result = timeout_at(deadline, read_frame(stream, MAX_FRAME)).await?;
    let receipt = match result {
        Ok(bytes) => match decode(&bytes) {
            Ok(request) => match blocking.try_acquire_owned() {
                Ok(permit) => {
                    let worker = tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        if Instant::now() >= deadline { return Err(Error::Rejected); }
                        dispatch(request, &*requests, deadline)
                    });
                    timeout_at(deadline, worker).await?.unwrap_or(Err(Error::Rejected))
                }
                Err(_) => Err(Error::Rejected),
            },
            Err(error) => Err(error),
        },
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidData | io::ErrorKind::FileTooLarge
            ) =>
        {
            Err(Error::InvalidRequest)
        }
        Err(error) => return Err(error),
    }
    .unwrap_or_else(|error| Receipt::Failed { error });
    let encoded = encode_frame(&receipt, MAX_FRAME)
        .unwrap_or_else(|_| br#"{"error":"rejected"}"#.to_vec());
    timeout_at(deadline, write_frame(stream, &encoded, MAX_FRAME)).await?
}

fn dispatch(request: Request, requests: &impl Requests, deadline: Instant) -> Result<Receipt> {
    let result = match request {
        Request::List {} => requests
            .list()
            .map(|connections| Receipt::Listed { connections }),
        Request::Draft { draft } => requests
            .open_draft(validate_draft(draft)?, deadline)
            .map(|draft_id| Receipt::Drafted { draft_id }),
        Request::Propose { proposal } => {
            proposal.validate()?;
            requests
                .request_review(proposal, deadline)
                .map(|proposal_id| Receipt::Proposed { proposal_id })
        }
    };
    result.map_err(|_| Error::Rejected)
}

fn validate_draft(draft: Draft) -> Result<ValidatedDraft> {
    let draft = draft.validate().map_err(|_| Error::InvalidRequest)?;
    if matches!(draft.method(), connections::profile::MethodRef::Ssh { .. }) {
        return Err(Error::InvalidRequest);
    }
    Ok(draft)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::InvalidRequest)?;
    serde_json::from_str(text).map_err(|_| Error::InvalidRequest)
}

fn parse(command: &str, input: impl Read) -> Result<Outbound> {
    if command == "list" {
        return Ok(Outbound::List);
    }
    if !matches!(command, "draft" | "propose") {
        return Err(Error::Usage);
    }
    let mut bytes = Vec::new();
    input
        .take((MAX_STDIN + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::InvalidRequest)?;
    if bytes.len() > MAX_STDIN {
        return Err(Error::InvalidRequest);
    }
    if command == "draft" {
        let draft: Draft = decode(&bytes)?;
        Ok(Outbound::Draft {
            draft: validate_draft(draft)?,
        })
    } else {
        let proposal: Proposal = decode(&bytes)?;
        proposal.validate()?;
        Ok(Outbound::Propose { proposal })
    }
}

async fn exchange(endpoint: &Endpoint, request: &Outbound) -> Result<Receipt> {
    let bytes = encode_frame(request, MAX_FRAME).map_err(|_| Error::InvalidRequest)?;
    let mut stream = timeout(DEADLINE, endpoint.connect())
        .await
        .map_err(|_| Error::Unavailable)?
        .map_err(|_| Error::Unavailable)?;
    let reply = timeout(DEADLINE, async {
        write_frame(&mut stream, &bytes, MAX_FRAME).await?;
        read_frame(&mut stream, MAX_FRAME).await
    })
    .await
    .map_err(|_| Error::Unavailable)?
    .map_err(|error| {
        if matches!(
            error.kind(),
            io::ErrorKind::InvalidData | io::ErrorKind::FileTooLarge
        ) {
            Error::InvalidResponse
        } else {
            Error::Unavailable
        }
    })?;
    match (request, decode(&reply).map_err(|_| Error::InvalidResponse)?) {
        (Outbound::List, receipt @ Receipt::Listed { .. }) => Ok(receipt),
        (Outbound::Draft { .. }, receipt @ Receipt::Drafted { .. }) => Ok(receipt),
        (Outbound::Propose { .. }, receipt @ Receipt::Proposed { .. }) => Ok(receipt),
        (
            _,
            Receipt::Failed {
                error: error @ (Error::InvalidRequest | Error::Rejected),
            },
        ) => Err(error),
        _ => Err(Error::InvalidResponse),
    }
}

#[doc(hidden)]
pub fn run() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    let result = (|| {
        let command = args.next().ok_or(Error::Usage)?;
        if args.next().is_some() {
            return Err(Error::Usage);
        }
        let request = parse(command.to_str().ok_or(Error::Usage)?, io::stdin().lock())?;
        let endpoint = Endpoint::for_service(SERVICE).map_err(|_| Error::Unavailable)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::Unavailable)?;
        let receipt = runtime.block_on(exchange(&endpoint, &request))?;
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, &receipt).map_err(|_| Error::InvalidResponse)?;
        writeln!(stdout).map_err(|_| Error::InvalidResponse)
    })();
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::from(if matches!(error, Error::Usage | Error::InvalidRequest) {
                2
            } else {
                1
            })
        }
    }
}

#[cfg(test)]
mod tests;
