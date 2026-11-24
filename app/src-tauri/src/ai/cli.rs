use std::{
    fmt,
    future::Future,
    io::{self, Read, Write},
    path::Path,
    sync::Arc,
    num::NonZeroU64,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Instant, timeout, timeout_at},
};

use crate::{
    connections::{self, profile::{Draft, MethodRef, SshAuth, SshTransport, ValidatedDraft, valid_id}},
    editor::{self, cli as edit},
    local_ipc::{self, DEADLINE, Endpoint, Listener, MAX_FRAME, SERVICE, encode_frame, read_frame, write_frame},
};
use tauri::{AppHandle, Manager};

use super::ipc::State;

const MAX_STDIN: usize = MAX_FRAME - 64;
const MAX_EDITS: usize = 32;
const USAGE: &str = "사용법: zzabba list | zzabba register | zzabba propose";

type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Error {
    Usage,
    InvalidRequest,
    Unavailable,
    InvalidResponse,
    Rejected,
    #[serde(skip)]
    StoreUnavailable,
    #[serde(skip)]
    GroupConflict,
    #[serde(skip)]
    TargetConflict,
    #[serde(skip)]
    SshOverSsmUnavailable,
    #[serde(skip)]
    StoredSshPasswordUnavailable,
    #[cfg(windows)]
    #[serde(skip)]
    UnsupportedWindowsStartDirectory,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Usage => USAGE,
            Self::InvalidRequest => "잘못된 요청입니다.",
            Self::Unavailable => "ZzabbaXterm 앱에 연결할 수 없습니다.",
            Self::InvalidResponse => "ZzabbaXterm 앱의 응답이 올바르지 않습니다.",
            Self::Rejected => "ZzabbaXterm 앱이 요청을 거부했습니다.",
            Self::StoreUnavailable => "저장된 연결 설정에 접근할 수 없습니다.",
            Self::GroupConflict => "저장된 연결 그룹이 입력과 일치하지 않습니다.",
            Self::TargetConflict => "새 대상 ID와 비어 있는 대상 개정이 필요합니다.",
            Self::SshOverSsmUnavailable => "SSH-over-SSM을 사용할 수 없습니다",
            Self::StoredSshPasswordUnavailable => "저장된 SSH 암호를 사용할 수 없습니다",
            #[cfg(windows)]
            Self::UnsupportedWindowsStartDirectory => "작업 디렉터리: Windows 드라이브의 일반 절대 경로만 지원합니다",
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
    Propose { proposal: Proposal },
    Edit(edit::Request),
}

#[derive(Serialize)]
#[serde(tag = "method", rename_all = "camelCase")]
enum Outbound {
    List,
    Propose { proposal: Proposal },
}

enum Command {
    Register(ValidatedDraft),
    Request(Outbound),
}

#[derive(Serialize)]
struct Registration {
    id: String,
    generation: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Output {
    Registered(Registration),
    Reply(Receipt),
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
    Proposed {
        #[serde(rename = "proposalId")]
        proposal_id: String,
    },
    Failed {
        error: Error,
    },
}

// 연결 조회와 제안 접수에는 승인이나 실행 권한이 없다.
pub(crate) trait Requests {
    fn list(&self) -> Result<Vec<Alias>, Rejected>;
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
            Ok(listener) => {
                let editor_app = app.clone();
                serve(listener, AppRequests(app), move |pending| {
                    editor::prepare_cli(editor_app.clone(), pending)
                }).await
            }
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

pub(crate) async fn serve<F, Work>(
    listener: Listener,
    requests: impl Requests + Send + Sync + 'static,
    confirm: F,
) -> io::Result<()>
where
    F: Fn(edit::Pending) -> Work + Clone + Send + 'static,
    Work: Future<Output = ()> + Send + 'static,
{
    let requests = Arc::new(requests);
    let blocking = Arc::new(Semaphore::new(4));
    let edits = Arc::new(Semaphore::new(MAX_EDITS));
    let shutdown = tokio_util::sync::CancellationToken::new();
    let _cancel_edits = shutdown.clone().drop_guard();
    local_ipc::serve(listener, move |mut stream| {
        let requests = Arc::clone(&requests);
        let blocking = Arc::clone(&blocking);
        let edits = Arc::clone(&edits);
        let confirm = confirm.clone();
        let shutdown = shutdown.clone();
        async move {
            if let Ok(Some((request, permit))) =
                serve_connection(&mut stream, requests, blocking, &edits).await
            {
                // 접수된 편집은 일반 연결 슬롯을 반환하고 전용 슬롯만 유지한다.
                tokio::spawn(async move {
                    tokio::select! {
                        _ = shutdown.cancelled() => {},
                        _ = edit::serve_edit(&mut stream, request, confirm) => {},
                    }
                    drop(permit);
                    drop(stream);
                });
            }
        }
    }).await
}

async fn serve_connection(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    requests: Arc<impl Requests + Send + Sync + 'static>,
    blocking: Arc<Semaphore>,
    edits: &Arc<Semaphore>,
) -> io::Result<Option<(edit::Request, OwnedSemaphorePermit)>> {
    let deadline = Instant::now() + DEADLINE;
    let result = timeout_at(deadline, read_frame(stream, MAX_FRAME)).await?;
    let receipt = match result {
        Ok(bytes) => match decode(&bytes) {
            Ok(Request::Edit(request)) => {
                if !request.is_valid() {
                    edit::send_edit(stream, edit::Response::InvalidRequest, deadline).await?;
                    return Ok(None);
                }
                #[cfg(windows)]
                if !matches!(Listener::current_process_is_elevated(), Ok(false)) {
                    edit::send_edit(stream, edit::Response::OpenFailed, deadline).await?;
                    return Ok(None);
                }
                let Ok(permit) = Arc::clone(edits).try_acquire_owned() else {
                    edit::send_edit(stream, edit::Response::OpenFailed, deadline).await?;
                    return Ok(None);
                };
                edit::send_edit(stream, edit::Response::Accepted, deadline).await?;
                return Ok(Some((request, permit)));
            }
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
    timeout_at(deadline, write_frame(stream, &encoded, MAX_FRAME)).await??;
    Ok(None)
}

fn dispatch(request: Request, requests: &impl Requests, deadline: Instant) -> Result<Receipt> {
    let result = match request {
        Request::List {} => requests
            .list()
            .map(|connections| Receipt::Listed { connections }),
        Request::Propose { proposal } => {
            proposal.validate()?;
            requests
                .request_review(proposal, deadline)
                .map(|proposal_id| Receipt::Proposed { proposal_id })
        }
        Request::Edit(_) => return Err(Error::InvalidRequest),
    };
    result.map_err(|_| Error::Rejected)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::InvalidRequest)?;
    serde_json::from_str(text).map_err(|_| Error::InvalidRequest)
}

fn parse(command: &str, input: impl Read) -> Result<Command> {
    if command == "list" {
        return Ok(Command::Request(Outbound::List));
    }
    if !matches!(command, "register" | "propose") {
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
    if command == "register" {
        let draft: Draft = decode(&bytes)?;
        let draft = draft.validate().map_err(|_| Error::InvalidRequest)?;
        match draft.method() {
            MethodRef::Ssh { ssh } => {
                if matches!(ssh.transport, SshTransport::Ssm(_)) {
                    return Err(Error::SshOverSsmUnavailable);
                }
                if matches!(ssh.auth, SshAuth::StoredPassword { .. }) {
                    return Err(Error::StoredSshPasswordUnavailable);
                }
            }
            #[cfg(windows)]
            MethodRef::Local { start_directory, .. } => {
                use std::path::{Component, Prefix};
                // 빈 경로는 홈 디렉터리에서 시작하므로 드라이브 경로 검사를 건너뛴다.
                if !start_directory.as_os_str().is_empty()
                    && !matches!(start_directory.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))) {
                    return Err(Error::UnsupportedWindowsStartDirectory);
                }
            }
            #[cfg(not(windows))]
            MethodRef::Local { .. } => {}
            MethodRef::Ssm { .. } | MethodRef::S3 { .. } | MethodRef::Desktop { .. } => {}
        }
        Ok(Command::Register(draft))
    } else {
        let proposal: Proposal = decode(&bytes)?;
        proposal.validate()?;
        Ok(Command::Request(Outbound::Propose { proposal }))
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

fn register(directory: &Path, draft: ValidatedDraft) -> Result<Output> {
    let (id, generation) = connections::register(directory, draft)?;
    Ok(Output::Registered(Registration { id, generation }))
}

#[doc(hidden)]
pub fn run() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    let result = (|| {
        let command = args.next().ok_or(Error::Usage)?;
        if args.next().is_some() {
            return Err(Error::Usage);
        }
        let output = match parse(command.to_str().ok_or(Error::Usage)?, io::stdin().lock())? {
            Command::Register(draft) => {
                let directory = dirs::config_dir().ok_or(Error::StoreUnavailable)?.join(env!("ZZABBA_APP_IDENTIFIER"));
                register(&directory, draft)?
            }
            Command::Request(request) => {
                let endpoint = Endpoint::for_service(SERVICE).map_err(|_| Error::Unavailable)?;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| Error::Unavailable)?;
                Output::Reply(runtime.block_on(exchange(&endpoint, &request))?)
            }
        };
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, &output).map_err(|_| Error::InvalidResponse)?;
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
