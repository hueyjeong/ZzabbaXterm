use super::{AwsCli, CancellationToken, CliError, Scope};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tauri::{
    Manager, RunEvent, Runtime, Url, Webview, WindowEvent,
    ipc::{Invoke, InvokeBody},
    plugin::{Builder, TauriPlugin},
    webview::PageLoadEvent,
};

pub const COMMAND: &str = "ssm_discovery";
const REQUEST_LIMIT: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
enum Code {
    InvalidRequest,
    RequestLimit,
    Cancelled,
    AwsCliMissing,
    AwsCliNotV2,
    InvalidProfile,
    ProfileNotFound,
    UnsupportedProfile,
    InvalidRegion,
    InvalidConfiguration,
    CredentialsUnavailable,
    CredentialsExpired,
    SsoLoginRequired,
    SsoLoginNotApplicable,
    AccessDenied,
    EndpointUnavailable,
    TimedOut,
    Unknown,
}

impl From<CliError> for Code {
    fn from(error: CliError) -> Self {
        match error {
            CliError::Cancelled => Self::Cancelled,
            CliError::AwsCliMissing => Self::AwsCliMissing,
            CliError::AwsCliNotV2 => Self::AwsCliNotV2,
            CliError::InvalidProfile => Self::InvalidProfile,
            CliError::ProfileNotFound => Self::ProfileNotFound,
            CliError::UnsupportedProfile => Self::UnsupportedProfile,
            CliError::InvalidRegion => Self::InvalidRegion,
            CliError::InvalidConfiguration => Self::InvalidConfiguration,
            CliError::CredentialsUnavailable => Self::CredentialsUnavailable,
            CliError::CredentialsExpired => Self::CredentialsExpired,
            CliError::SsoLoginRequired => Self::SsoLoginRequired,
            CliError::SsoLoginNotApplicable => Self::SsoLoginNotApplicable,
            CliError::AccessDenied => Self::AccessDenied,
            CliError::EndpointUnavailable => Self::EndpointUnavailable,
            CliError::TimedOut => Self::TimedOut,
            _ => Self::Unknown,
        }
    }
}

#[derive(Serialize)]
struct Failure {
    code: Code,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request<'a> {
    document: &'a str,
    #[serde(borrow)]
    request: Action<'a>,
}

#[derive(Deserialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Action<'a> {
    Reserve,
    Run {
        request_id: &'a str,
        operation: Operation,
    },
    Relogin {
        request_id: &'a str,
    },
    Cancel {
        request_id: &'a str,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum Operation {
    ListProfiles,
    ConfiguredRegion { profile: String },
    ListOnlineNodes { profile: String, region: String },
}

#[derive(Serialize)]
#[serde(untagged)]
enum Value {
    Empty,
    Profiles(Vec<String>),
    Region(Option<String>),
    Nodes(Vec<Node>),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Node {
    instance_id: String,
}

#[derive(Clone, Default)]
struct Discovery(Arc<Mutex<Requests>>);

#[derive(Default)]
struct Requests {
    next_id: u64,
    documents: HashMap<String, Document>,
}

struct Document {
    key: String,
    requests: HashMap<u64, Phase>,
}

impl Drop for Document {
    fn drop(&mut self) {
        for phase in self.requests.values() {
            if let Phase::Running(cancel) = phase {
                cancel.cancel();
            }
        }
    }
}

enum Phase {
    Reserved,
    Running(CancellationToken),
    Expired(Scope),
}

enum Work {
    Discover(Operation),
    Relogin(Scope),
}

impl Requests {
    fn document(&mut self, owner: &str, key: &str) -> Result<&mut Document, Code> {
        self.documents
            .get_mut(owner)
            .filter(|doc| doc.key == key)
            .ok_or(Code::Cancelled)
    }

    fn reserve(&mut self, owner: &str, key: &str) -> Result<u64, Code> {
        let id = self.next_id.checked_add(1).ok_or(Code::RequestLimit)?;
        let doc = self.document(owner, key)?;
        if doc.requests.len() >= REQUEST_LIMIT {
            return Err(Code::RequestLimit);
        }
        doc.requests.insert(id, Phase::Reserved);
        self.next_id = id;
        Ok(id)
    }

    fn start(
        &mut self,
        owner: &str,
        key: &str,
        id: u64,
        operation: Option<Operation>,
    ) -> Result<(Work, CancellationToken), Code> {
        let slot = self
            .document(owner, key)?
            .requests
            .get_mut(&id)
            .ok_or(Code::Cancelled)?;
        match (&operation, &*slot) {
            (Some(_), Phase::Reserved) | (None, Phase::Expired(_)) => {}
            (None, _) => return Err(Code::SsoLoginNotApplicable),
            (Some(_), _) => return Err(Code::InvalidRequest),
        }
        let cancel = CancellationToken::new();
        let previous = std::mem::replace(slot, Phase::Running(cancel.clone()));
        let work = match previous {
            Phase::Expired(scope) => Work::Relogin(scope),
            _ => Work::Discover(operation.expect("reserved operation")),
        };
        Ok((work, cancel))
    }

    fn cancel(&mut self, owner: &str, key: &str, id: u64) -> Result<(), Code> {
        if let Some(Phase::Running(cancel)) = self.document(owner, key)?.requests.remove(&id) {
            cancel.cancel();
        }
        Ok(())
    }

    fn finish(
        &mut self,
        owner: &str,
        key: &str,
        id: u64,
        expired: Option<Scope>,
    ) -> Result<(), Code> {
        let requests = &mut self.document(owner, key)?.requests;
        if !matches!(requests.get(&id), Some(Phase::Running(_))) {
            return Err(Code::Cancelled);
        }
        if let Some(scope) = expired {
            requests.insert(id, Phase::Expired(scope));
        } else {
            requests.remove(&id);
        }
        Ok(())
    }
}

async fn execute(
    work: Work,
    cancel: &CancellationToken,
) -> (Result<Value, CliError>, Option<Scope>) {
    if cancel.is_cancelled() {
        return (Err(CliError::Cancelled), None);
    }
    let cli = match AwsCli::probe(cancel).await {
        Ok(cli) => cli,
        Err(error) => return (Err(error), None),
    };
    let result = match work {
        Work::Discover(Operation::ListProfiles) => cli.profiles(cancel).await.map(Value::Profiles),
        Work::Discover(Operation::ConfiguredRegion { profile }) => cli
            .configured_region(&profile, cancel)
            .await
            .map(Value::Region),
        Work::Discover(Operation::ListOnlineNodes { profile, region }) => {
            let scope = match Scope::new(profile, region) {
                Ok(scope) => scope,
                Err(error) => return (Err(error), None),
            };
            match cli.online_nodes(&scope, cancel).await {
                // 재로그인 권한은 호출자의 오류 인자가 아니라 이 조회 결과에서 발급한다.
                Err(CliError::SsoLoginRequired) => {
                    return (Err(CliError::SsoLoginRequired), Some(scope));
                }
                result => result.map(|nodes| {
                    Value::Nodes(
                        nodes
                            .into_iter()
                            .map(|node| Node {
                                instance_id: node.instance_id,
                            })
                            .collect(),
                    )
                }),
            }
        }
        Work::Relogin(scope) => cli
            .relogin(&scope, CliError::SsoLoginRequired, cancel)
            .await
            .map(|()| Value::Empty),
    };
    (result, None)
}

enum Prepared {
    Reserved(u64),
    Cancelled,
    Run {
        state: Discovery,
        owner: String,
        key: String,
        id: u64,
        work: Work,
        cancel: CancellationToken,
    },
}

/// ACL 검사를 통과한 앱 명령만 동기적으로 등록하고 CLI 작업을 넘긴다.
pub fn dispatch<R: Runtime>(invoke: Invoke<R>) {
    let Invoke {
        message, resolver, ..
    } = invoke;
    let prepared = (|| {
        let caller = message.webview_ref();
        let owner = owner(caller).ok_or(Code::Cancelled)?;
        let InvokeBody::Json(payload) = message.payload() else {
            return Err(Code::InvalidRequest);
        };
        let Request {
            document: key,
            request,
        } = Request::deserialize(payload).map_err(|_| Code::InvalidRequest)?;
        let discovery = caller.state::<Discovery>();
        let mut requests = discovery.0.lock().unwrap();
        let (id, operation) = match request {
            Action::Reserve => return requests.reserve(owner, key).map(Prepared::Reserved),
            Action::Cancel { request_id } => {
                return requests
                    .cancel(owner, key, parse_id(request_id)?)
                    .map(|()| Prepared::Cancelled);
            }
            Action::Run {
                request_id,
                operation,
            } => (parse_id(request_id)?, Some(operation)),
            Action::Relogin { request_id } => (parse_id(request_id)?, None),
        };
        let (work, cancel) = requests.start(owner, key, id, operation)?;
        Ok(Prepared::Run {
            state: discovery.inner().clone(),
            owner: owner.to_owned(),
            key: key.to_owned(),
            id,
            work,
            cancel,
        })
    })();
    match prepared {
        Err(code) => resolver.reject(Failure { code }),
        Ok(Prepared::Reserved(id)) => resolver.resolve(id.to_string()),
        Ok(Prepared::Cancelled) => resolver.resolve(()),
        Ok(Prepared::Run {
            state,
            owner,
            key,
            id,
            work,
            cancel,
        }) => resolver.respond_async(async move {
            let (result, expired) = execute(work, &cancel).await;
            state
                .0
                .lock()
                .unwrap()
                .finish(&owner, &key, id, expired)
                .map_err(|code| Failure { code })?;
            result.map_err(|error| Failure { code: error.into() }.into())
        }),
    }
}

fn parse_id(id: &str) -> Result<u64, Code> {
    if id.is_empty() || id.starts_with('0') || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Code::InvalidRequest);
    }
    id.parse().map_err(|_| Code::InvalidRequest)
}

fn owner<R: Runtime>(webview: &Webview<R>) -> Option<&str> {
    // 현재 계약은 WebviewWindow의 자체 문서만 허용한다.
    (webview.window_ref().label() == webview.label()).then(|| webview.label())
}

fn local_document<R: Runtime>(webview: &Webview<R>, url: &Url) -> bool {
    if tauri::is_dev() {
        if let Some(dev) = &webview.config().build.dev_url {
            return url.origin() == dev.origin();
        }
    }
    if cfg!(any(windows, target_os = "android")) {
        matches!(url.scheme(), "http" | "https")
            && url.host_str() == Some("tauri.localhost")
            && url.port().is_none()
    } else {
        url.scheme() == "tauri" && url.host_str() == Some("localhost") && url.port().is_none()
    }
}

/// 앱 명령의 등록과 별도로 문서 전환, 창 종료 및 앱 종료를 추적한다.
pub fn lifecycle<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("ssm-discovery-lifecycle")
        .setup(|app, _| {
            app.manage(Discovery::default());
            Ok(())
        })
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started {
                if let Some(owner) = owner(webview) {
                    webview.state::<Discovery>().0.lock().unwrap().documents.remove(owner);
                }
                return;
            }
            let Some(owner) = owner(webview) else { return; };
            let state = webview.state::<Discovery>();
            let mut requests = state.0.lock().unwrap();
            if !local_document(webview, payload.url()) {
                return;
            }
            let mut nonce = [0; 16];
            if getrandom::fill(&mut nonce).is_err() {
                return;
            }
            let key = format!("{:032x}", u128::from_ne_bytes(nonce));
            // 문서마다 새 키를 주입한다. JS가 보낸 창 이름이나 세대 값으로 새 문서를 등록하지 않는다.
            let script = format!(
                "window.__ZZABBA_SSM_DOCUMENT__ = '{key}'; window.dispatchEvent(new Event('zzabba:ssm-document'));"
            );
            requests.documents.insert(owner.to_owned(), Document { key, requests: HashMap::new() });
            drop(requests);
            if webview.eval(script).is_err() {
                state.0.lock().unwrap().documents.remove(owner);
            }
        })
        .on_event(|app, event| match event {
            RunEvent::WindowEvent { label, event: WindowEvent::Destroyed, .. } => {
                app.state::<Discovery>().0.lock().unwrap().documents.remove(label);
            }
            RunEvent::Exit => app.state::<Discovery>().0.lock().unwrap().documents.clear(),
            _ => {}
        })
        .build()
}

#[cfg(test)]
mod tests;
