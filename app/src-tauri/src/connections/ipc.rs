use serde::{Serialize, de::DeserializeOwned};
use std::{fs, path::Path};
use tauri::{Manager, Webview, ipc::{CommandArg, CommandItem, Invoke, InvokeError}};

use crate::terminal::Sessions;
use super::{
    probe,
    profile::{GroupDraft, Revision, TargetDraft, ValidationError},
    store::{Expected, SaveStatus, Store, StoreError},
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
    stored_auth: bool,
}

#[derive(Serialize)]
struct Saved {
    revision: Revision,
}

/// 그룹과 대상 편집 명령의 오류다. 편집 창이 필드 옆이나 상태 줄에 표시한다.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum EditError {
    Invalid { field: &'static str },
    Conflict,
    Referenced { targets: Vec<String> },
    Failed { message: String },
}

impl From<ValidationError> for EditError {
    fn from(error: ValidationError) -> Self {
        Self::Invalid { field: error.field() }
    }
}

impl From<String> for EditError {
    fn from(message: String) -> Self {
        Self::Failed { message }
    }
}

/// `missing_group`은 없는 그룹을 가리킨 입력 필드다. 그룹 저장은 `parentId`, 대상 저장은 `groupId`다.
fn edited(result: Result<SaveStatus, StoreError>, missing_group: &'static str) -> Result<(), EditError> {
    match result {
        Ok(SaveStatus::Saved) => Ok(()),
        Ok(SaveStatus::GroupConflict | SaveStatus::TargetConflict) => Err(EditError::Conflict),
        Ok(SaveStatus::Referenced(targets)) => Err(EditError::Referenced { targets }),
        Err(StoreError::InvalidProfile(error)) => Err(error.into()),
        Err(StoreError::MissingGroup) => Err(EditError::Invalid { field: missing_group }),
        Err(StoreError::GroupCycle | StoreError::GroupDepth) => Err(EditError::Invalid { field: "parentId" }),
        Err(StoreError::DesktopRoute) => Err(EditError::Invalid { field: "desktop.route" }),
        Err(StoreError::Authorization(reason)) => Err(EditError::Failed { message: reason }),
        Err(other) => Err(EditError::Failed { message: format!("프로필 저장: {other:?}") }),
    }
}

fn next(revision: Option<Revision>) -> Result<Revision, EditError> {
    revision.ok_or_else(|| EditError::Failed { message: "프로필 저장: RevisionExhausted".into() })
}

/// SSM과 S3 세션을 시험 없이 저장할 때 기록하는 로컬 SSO 설정의 계정이다. 네트워크는 쓰지 않는다.
fn configured_account(profile: &str, region: &str) -> Option<String> {
    let scope = crate::ssm::Scope::new(profile.into(), region.into()).ok()?;
    scope.direct_sso_config().ok().map(|config| config.account)
}

#[cfg(test)]
mod capabilities_tests {
    use super::*;

    #[test]
    fn edit_errors_serialize_as_kind_and_field() {
        use serde_json::json;
        let value = |error: EditError| serde_json::to_value(error).unwrap();
        assert_eq!(value(ValidationError::SshPort.into()), json!({ "kind": "invalid", "field": "ssh.port" }));
        assert_eq!(value(EditError::Conflict), json!({ "kind": "conflict" }));
        assert_eq!(value(EditError::Referenced { targets: vec!["rdp".into()] }), json!({ "kind": "referenced", "targets": ["rdp"] }));
        assert_eq!(edited(Err(StoreError::GroupDepth), "parentId"), Err(EditError::Invalid { field: "parentId" }));
        assert_eq!(edited(Err(StoreError::MissingGroup), "groupId"), Err(EditError::Invalid { field: "groupId" }));
    }

    #[test]
    fn capabilities_serialize_stored_auth_as_camel_case() {
        let value = serde_json::to_value(Capabilities { stored_auth: true }).unwrap();
        assert_eq!(value, serde_json::json!({ "storedAuth": true }));
        assert!(value.get("stored_auth").is_none());
        let value = serde_json::to_value(Capabilities { stored_auth: false }).unwrap();
        assert_eq!(value, serde_json::json!({ "storedAuth": false }));
    }
}

pub(crate) fn handle(invoke: Invoke, other: fn(Invoke) -> bool) -> bool {
    match invoke.message.command() {
        "connection_capabilities" => dispatch(invoke, "connection_capabilities", |_| {
            Ok(|_, _| Ok::<_, String>(Capabilities {
                stored_auth: cfg!(all(feature = "stored-auth", any(windows, target_os = "linux"))),
            }))
        }),
        "connection_test_start" => dispatch(invoke, "connection_test_start", |args| {
            let draft_id = args.get("draftId")?;
            let draft_version = args.get("draftVersion")?;
            let draft = args.get("draft")?;
            Ok(move |caller: Webview, document| {
                let config = caller.path().app_config_dir().map_err(|error| format!("앱 설정 디렉터리: {error}"))?;
                probe::start(&caller.state::<Sessions>(), caller.label(), document, draft_id, draft_version, draft, config)
            })
        }),
        crate::ssh::registration::COMMAND | crate::ssh::registration::COMMIT | crate::ssh::registration::CANCEL => {
            crate::ssh::registration::dispatch(invoke)
        }
        "register_saved_ssh_host" => dispatch(invoke, "register_saved_ssh_host", |args| {
            let target_id: String = args.get("targetId")?;
            Ok(move |caller: Webview, document| crate::ssh::registration::register_saved(&caller, document, target_id))
        }),
        "connection_ssh_test_attach" => dispatch(invoke, "connection_ssh_test_attach", |args| {
            let run = args.get("run")?;
            let rows = args.get("rows")?;
            let cols = args.get("cols")?;
            let output = args.channel("output")?;
            Ok(move |caller: Webview, document| probe::attach(&caller.state::<Sessions>(), caller.label(), document, &run, rows, cols, output))
        }),
        "connection_test_wait" => dispatch(invoke, "connection_test_wait", |args| {
            let run = args.get("run")?;
            Ok(move |caller: Webview, document| probe::wait(&caller.state::<Sessions>(), caller.label(), document, &run))
        }),
        "connection_test_cancel" => dispatch(invoke, "connection_test_cancel", |args| {
            let run = args.get("run")?;
            Ok(move |caller: Webview, document| probe::cancel(&caller.state::<Sessions>(), caller.label(), document, &run))
        }),
        "connection_save" => dispatch(invoke, "connection_save", |args| {
            let request = args.get("request")?;
            Ok(move |caller: Webview, document| {
                probe::save_confirmed(&caller.state::<Sessions>(), caller.label(), document, request, &store(&caller)?)
            })
        }),
        "connection_group_save" => dispatch(invoke, "connection_group_save", |args| {
            let draft: GroupDraft = args.get("draft")?;
            Ok(move |caller: Webview, document| -> Result<Saved, EditError> {
                let draft = draft.validate()?;
                let revision = next(draft.next_revision())?;
                let sessions = caller.state::<Sessions>();
                let authorize = || sessions.with_probes(caller.label(), document, |_| Ok(()));
                edited(store(&caller)?.save_group(draft, authorize), "parentId")?;
                Ok(Saved { revision })
            })
        }),
        "connection_group_delete" => dispatch(invoke, "connection_group_delete", |args| {
            let id: String = args.get("id")?;
            let revision: Revision = args.get("revision")?;
            let expected: Vec<Expected> = args.get("expected")?;
            Ok(move |caller: Webview, document| -> Result<(), EditError> {
                let sessions = caller.state::<Sessions>();
                let authorize = || sessions.with_probes(caller.label(), document, |_| Ok(()));
                edited(store(&caller)?.delete_group(&id, revision, expected, authorize), "parentId")
            })
        }),
        "connection_target_delete" => dispatch(invoke, "connection_target_delete", |args| {
            let id: String = args.get("id")?;
            let revision: Revision = args.get("revision")?;
            Ok(move |caller: Webview, document| -> Result<(), EditError> {
                let sessions = caller.state::<Sessions>();
                let authorize = || sessions.with_probes(caller.label(), document, |_| Ok(()));
                edited(store(&caller)?.delete_target(&id, revision, authorize), "groupId")
            })
        }),
        "connection_target_save" => dispatch(invoke, "connection_target_save", |args| {
            let draft: TargetDraft = args.get("draft")?;
            Ok(move |caller: Webview, document| -> Result<Saved, EditError> {
                let draft = draft.validate()?;
                let revision = next(draft.next_revision())?;
                let sessions = caller.state::<Sessions>();
                let authorize = || sessions.with_probes(caller.label(), document, |_| Ok(()));
                edited(store(&caller)?.save_validated(draft, configured_account, authorize), "groupId")?;
                Ok(Saved { revision })
            })
        }),
        "connections_load" => dispatch(invoke, "connections_load", |_| {
            Ok(|caller: Webview, document| {
                let sessions = caller.state::<Sessions>();
                let check = || sessions.with_probes(caller.label(), document, |_| Ok(()));
                check()?;
                let result = store(&caller)?.load();
                check()?;
                result.map_err(|error| format!("프로필 읽기: {error:?}"))
            })
        }),
        _ => return other(invoke),
    }
    true
}

struct Args<'a>(&'a Invoke, &'static str);

impl Args<'_> {
    fn get<T: DeserializeOwned>(&self, key: &'static str) -> Result<T, InvokeError> {
        let Invoke { message, acl, .. } = self.0;
        T::from_command(CommandItem { name: self.1, key, plugin: None, message, acl })
    }

    fn channel(&self, key: &'static str) -> Result<tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>, InvokeError> {
        let Invoke { message, acl, .. } = self.0;
        CommandArg::from_command(CommandItem { name: self.1, key, plugin: None, message, acl })
    }
}

fn dispatch<T, E, Job>(invoke: Invoke, name: &'static str, parse: impl FnOnce(Args<'_>) -> Result<Job, InvokeError>)
where
    T: Serialize + Send + 'static,
    E: Serialize + Send + 'static,
    Job: FnOnce(Webview, u64) -> Result<T, E> + Send + 'static,
{
    let caller = invoke.message.webview_ref();
    // Tauri의 로컬 ACL을 통과한 요청에서 세대를 먼저 읽고, 작업 스레드에서 다시 검사한다.
    if caller.label() != "main" || caller.window_ref().label() != "main" || invoke.acl.is_none() {
        invoke.resolver.reject("앱의 로컬 기본 창에서만 연결 프로필을 사용할 수 있습니다");
        return;
    }
    let document = caller.state::<Sessions>().document(caller.label());
    let job = match parse(Args(&invoke, name)) {
        Ok(job) => job,
        Err(error) => { invoke.resolver.invoke_error(error); return; }
    };
    let caller = caller.clone();
    invoke.resolver.respond_async(async move {
        tauri::async_runtime::spawn_blocking(move || job(caller, document)).await
            .map_err(|error| InvokeError::from(format!("연결 작업: {error}")))?
            .map_err(InvokeError::from)
    });
}

fn store(caller: &Webview) -> Result<Store, String> {
    let directory = caller.path().app_config_dir().map_err(|error| format!("앱 설정 디렉터리: {error}"))?;
    store_at(&directory)
}

pub(crate) fn initialize_ssh_registration(app: &tauri::App) -> Result<crate::ssh::registration::Registration, String> {
    use crate::ssh::{Error, TrustStore, registration::Registration};
    let config = app.path().app_config_dir().map_err(|error| format!("앱 설정 디렉터리: {error}"))?;
    store_at(&config)?.load().map_err(|error| format!("프로필 읽기: {error:?}"))?;
    let root = config.join(crate::ssh::TRUST_DIRECTORY);
    let store = match TrustStore::open(&root) {
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => TrustStore::create(&root),
        result => result,
    }.map_err(|error| format!("SSH 신뢰 저장소: {error:?}"))?;
    Ok(Registration::new(store))
}

fn store_at(directory: &Path) -> Result<Store, String> {
    prepare_config_parent(directory)?;
    Ok(Store::new(directory))
}

pub(super) fn prepare_config_parent(directory: &Path) -> Result<(), String> {
    let parent = directory.parent().filter(|_| directory.is_absolute() && directory.file_name().is_some())
        .ok_or("앱 설정 디렉터리 경로가 올바르지 않습니다")?;
    // 설정 경로는 Tauri가 결정한다. 앱 디렉터리 자체는 Store가 생성하고 검사한다.
    fs::create_dir_all(parent).map_err(|error| format!("사용자 설정 디렉터리: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::tests::TestDir;

    #[test]
    fn c01_first_load_initializes_only_the_config_parent_before_store_runs() {
        let temp = TestDir::new();
        let directory = temp.path().join("missing").join("config").join("dev.zzabbaxterm.desktop");
        let store = store_at(&directory).unwrap();
        assert!(directory.parent().unwrap().is_dir());
        assert!(!directory.exists());
        assert!(store.load().unwrap().targets.is_empty());
        assert!(directory.is_dir());
    }
}
