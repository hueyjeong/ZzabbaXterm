use serde::{Serialize, de::DeserializeOwned};
use std::{fs, path::Path};
use tauri::{Manager, Webview, ipc::{CommandArg, CommandItem, Invoke, InvokeError}};

use crate::terminal::Sessions;
use super::{
    probe,
    profile::{GroupDraft, MethodRef, Revision, SshAuth, TargetDraft, ValidatedTarget, ValidationError},
    store::{Expected, SaveStatus, Store, StoreError},
};

/// 저장 암호를 쓸 수 있는 빌드인지다. 편집 창에 알리는 기능과 저장 검증이 같은 값을 쓴다.
const STORED_AUTH: bool = cfg!(all(feature = "stored-auth", any(windows, target_os = "linux")));

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
    stored_auth: bool,
}

#[derive(Serialize)]
struct Saved {
    revision: Revision,
}

/// 그룹과 대상 편집 명령, 시험 뒤 저장(`connection_save`)의 오류다. 편집 창이 필드 옆이나 상태 줄에 표시한다.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(super) enum EditError {
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
pub(super) fn edited(result: Result<SaveStatus, StoreError>, missing_group: &'static str) -> Result<(), EditError> {
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

/// 저장 암호 기능이 없는 빌드에서는 저장 암호를 참조하는 SSH 세션을 저장하지 않는다.
fn check_stored_auth(draft: &ValidatedTarget, stored_auth: bool) -> Result<(), EditError> {
    match draft.method() {
        MethodRef::Ssh { ssh } if !stored_auth && matches!(ssh.auth, SshAuth::StoredPassword { .. }) => {
            Err(ValidationError::SshItemId.into())
        }
        _ => Ok(()),
    }
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
    fn stored_password_targets_need_the_stored_auth_build() {
        use crate::connections::tests::{TestDir, ssh_draft, target_draft};
        use serde_json::json;
        let dir = TestDir::new();
        let validate = |input: &serde_json::Value| {
            serde_json::from_value::<TargetDraft>(target_draft(input)).unwrap().validate().unwrap()
        };
        let manual = ssh_draft(dir.path());
        let mut stored = manual.clone();
        stored["target"]["ssh"]["auth"] = json!({"kind": "storedPassword", "itemId": "21eb36c3-234d-439d-9e1f-1d3cc5ba8125"});
        assert_eq!(check_stored_auth(&validate(&stored), false), Err(EditError::Invalid { field: "ssh.auth.itemId" }));
        assert_eq!(check_stored_auth(&validate(&stored), true), Ok(()));
        assert_eq!(check_stored_auth(&validate(&manual), false), Ok(()));
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
                stored_auth: STORED_AUTH,
            }))
        }),
        "connection_test_start" => dispatch(invoke, "connection_test_start", |args| {
            let draft_id = args.get("draftId")?;
            let draft_version = args.get("draftVersion")?;
            let draft = args.get("draft")?;
            Ok(move |caller: Webview, document| {
                let config = caller.path().app_config_dir().map_err(|error| format!("앱 설정 디렉터리: {error}"))?;
                let trust = caller.app_handle().get_webview_window(caller.label()).map(crate::ssh::registration::host_key_trust);
                probe::start_trusting(&caller.state::<Sessions>(), caller.label(), document, draft_id, draft_version, draft, config, trust)
            })
        }),
        "register_saved_ssh_host" => dispatch(invoke, "register_saved_ssh_host", |args| {
            let target_id: String = args.get("targetId")?;
            Ok(move |caller: Webview, document| crate::ssh::registration::register_saved(&caller, document, target_id))
        }),
        "trust_saved_ssh_host" => dispatch(invoke, "trust_saved_ssh_host", |args| {
            let target_id: String = args.get("targetId")?;
            Ok(move |caller: Webview, document| crate::ssh::registration::trust_saved(&caller, document, target_id))
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
                delete_group(&config_dir(&caller)?, &id, revision, expected, authorize)
            })
        }),
        "connection_target_delete" => dispatch(invoke, "connection_target_delete", |args| {
            let id: String = args.get("id")?;
            let revision: Revision = args.get("revision")?;
            Ok(move |caller: Webview, document| -> Result<(), EditError> {
                let sessions = caller.state::<Sessions>();
                let authorize = || sessions.with_probes(caller.label(), document, |_| Ok(()));
                delete_target(&config_dir(&caller)?, &id, revision, authorize)
            })
        }),
        "connection_target_save" => dispatch(invoke, "connection_target_save", |args| {
            let draft: TargetDraft = args.get("draft")?;
            Ok(move |caller: Webview, document| -> Result<Saved, EditError> {
                let draft = draft.validate()?;
                check_stored_auth(&draft, STORED_AUTH)?;
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

/// 연결 대상을 지우고 그 대상을 전달 경로로 쓰던 RDP 계정 이름도 지운다.
pub(super) fn delete_target(directory: &Path, id: &str, revision: Revision, authorize: impl FnMut() -> Result<(), String>) -> Result<(), EditError> {
    edited(store_at(directory)?.delete_target(id, revision, authorize), "groupId")?;
    forget_rdp_accounts(directory, &[id]);
    Ok(())
}

/// 그룹과 하위 항목을 지우고 함께 지워진 연결 대상을 전달 경로로 쓰던 RDP 계정 이름도 지운다.
pub(super) fn delete_group(directory: &Path, id: &str, revision: Revision, expected: Vec<Expected>, authorize: impl FnMut() -> Result<(), String>) -> Result<(), EditError> {
    let removed: Vec<String> = expected.iter().filter_map(|entry| entry.target_id().map(str::to_owned)).collect();
    edited(store_at(directory)?.delete_group(id, revision, expected, authorize), "parentId")?;
    forget_rdp_accounts(directory, &removed.iter().map(String::as_str).collect::<Vec<_>>());
    Ok(())
}

/// 지운 연결 대상을 전달 경로로 쓰던 RDP 계정 이름을 지운다. 실패해도 삭제 결과는 바꾸지 않는다.
fn forget_rdp_accounts(directory: &Path, routes: &[&str]) {
    let _ = crate::rdp::accounts::forget(&directory.join(crate::rdp::accounts::FILE), routes);
}

fn config_dir(caller: &Webview) -> Result<std::path::PathBuf, String> {
    caller.path().app_config_dir().map_err(|error| format!("앱 설정 디렉터리: {error}"))
}

fn store(caller: &Webview) -> Result<Store, String> {
    store_at(&config_dir(caller)?)
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
