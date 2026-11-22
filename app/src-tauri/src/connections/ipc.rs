use serde::{Serialize, de::DeserializeOwned};
use std::{fs, path::Path};
use tauri::{Manager, Webview, ipc::{CommandArg, CommandItem, Invoke, InvokeError}};

use crate::terminal::Sessions;
use super::{probe, store::Store};

pub(crate) fn handle(invoke: Invoke, other: fn(Invoke) -> bool) -> bool {
    match invoke.message.command() {
        "connection_test_start" => dispatch(invoke, "connection_test_start", |args| {
            let draft_id = args.get("draftId")?;
            let draft_version = args.get("draftVersion")?;
            let draft = args.get("draft")?;
            Ok(move |caller: Webview, document| {
                let config = caller.path().app_config_dir().map_err(|error| format!("앱 설정 디렉터리: {error}"))?;
                probe::start(&caller.state::<Sessions>(), caller.label(), document, draft_id, draft_version, draft, config)
            })
        }),
        crate::ssh::registration::COMMAND => crate::ssh::registration::verify_ssh_host(invoke),
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

fn dispatch<T, Job>(invoke: Invoke, name: &'static str, parse: impl FnOnce(Args<'_>) -> Result<Job, InvokeError>)
where
    T: Serialize + Send + 'static,
    Job: FnOnce(Webview, u64) -> Result<T, String> + Send + 'static,
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
            .map_err(Into::into)
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
    let parent = directory.parent().filter(|_| directory.is_absolute() && directory.file_name().is_some())
        .ok_or("앱 설정 디렉터리 경로가 올바르지 않습니다")?;
    // 설정 경로는 Tauri가 결정한다. 앱 디렉터리 자체는 Store가 생성하고 검사한다.
    fs::create_dir_all(parent).map_err(|error| format!("사용자 설정 디렉터리: {error}"))?;
    Ok(Store::new(directory))
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
