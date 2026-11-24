use std::time::Instant;

use super::{Launch, Sessions, saved, shell, size, stage, validate_size};
use crate::ai::{execution, session_selection};
use tauri::{Manager, WebviewWindow, ipc::{Channel, CommandArg, CommandItem, Invoke, InvokeResponseBody}};

async fn blocking<T: Send + 'static>(job: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(job).await.map_err(|error| stage("worker", error))?
}

pub(crate) fn terminal_open(invoke: Invoke) {
    const COMMAND: &str = "terminal_open";
    let Invoke { message, resolver, acl } = invoke;
    // The generated async wrapper calls its command only after spawning the future.
    // Capture the generation in the synchronous, ACL-checked invoke handler instead.
    let caller = message.webview_ref();
    let document = caller.state::<Sessions>().document(caller.label());
    let argument = |key| CommandItem { name: COMMAND, key, plugin: None, message: &message, acl: &acl };
    let parse = || {
        Ok((
            WebviewWindow::from_command(argument("webview"))?,
            u16::from_command(argument("rows"))?,
            u16::from_command(argument("cols"))?,
            Channel::<InvokeResponseBody>::from_command(argument("output"))?,
            Option::<String>::from_command(argument("targetId"))?,
            Option::<bool>::from_command(argument("remoteEditor"))?,
        ))
    };
    let (webview, rows, cols, output, target_id, remote_editor) = match parse() {
        Ok(args) => args,
        Err(error) => { resolver.invoke_error(error); return; }
    };
    if target_id.is_some() && (caller.label() != "main" || caller.window_ref().label() != "main" || acl.is_none()) {
        resolver.reject("앱의 로컬 기본 창에서만 저장된 연결을 열 수 있습니다");
        return;
    }
    let remote_editor = remote_editor == Some(true);
    if remote_editor && target_id.is_none() {
        resolver.reject("nano 연동에는 저장된 SSH 연결이 필요합니다");
        return;
    }
    let window = target_id.is_some().then(|| caller.window());
    resolver.respond_async(async move {
        blocking(move || {
            let sessions = webview.state::<Sessions>();
            let owner = webview.label();
            let size = size(rows, cols);
            let send = move |event: super::Event| event.send(&output).map_err(|error| error.to_string());
            let Some(id) = target_id else {
                return sessions.open_document(owner, document, size, Launch::Local(shell()), send);
            };
            let directory = webview.path().app_config_dir().map_err(|error| stage("앱 설정 디렉터리", error))?;
            let Some(window) = window else {
                return sessions.open_document(owner, document, size, saved::launch(&directory, &id)?, send);
            };
            // 첫 연결의 서버 키 확인 창이다. SSM 경유 탭은 전달을 연 뒤 PTY 작업자에서 띄운다.
            let trust = crate::ssh::registration::host_key_trust(webview.clone());
            if !remote_editor {
                let launch = saved::launch_interactive(&directory, &id, owner, document, &trust, |message| {
                    crate::ssh::registration::confirm_stored_auth(&window, message)
                })?;
                return sessions.open_document(owner, document, size, launch, send);
            }
            validate_size(size)?;
            let (registration, command, parent) = saved::approve_remote_editor(&directory, &id, &trust, |message| {
                sessions.with_probe_access(owner, document, true, |_| Ok(()))?;
                crate::ssh::registration::confirm_remote_editor(&window, message)
            }, |mut launch| {
                let parent = launch.enable_editor(webview.app_handle().clone(), owner.into(), document);
                let command = Launch::from(launch);
                let registration = sessions.register_document(owner, document, &command, true)?;
                Ok((registration, command, parent))
            })?;
            let id = sessions.launch_document(registration, size, command, send)?;
            let _ = parent.send(id.clone());
            Ok(id)
        }).await.map_err(Into::into)
    });
}

#[tauri::command]
pub(crate) async fn terminal_start(webview: WebviewWindow, id: String) -> Result<(), String> {
    blocking(move || webview.state::<Sessions>().start(webview.label(), &id)).await
}

#[tauri::command]
pub(crate) async fn terminal_input(webview: WebviewWindow, id: String, bytes: Vec<u8>) -> Result<(), String> {
    blocking(move || {
        let sessions = webview.state::<Sessions>();
        if webview.label() == "main" && webview.as_ref().window().label() == "main" {
            let state = webview.state::<execution::State>().inner().clone();
            let now = Instant::now();
            if let Some(key) = state.observe_prefix(&id, &bytes, now) {
                let document = sessions.document("main");
                let confirmed = webview.state::<session_selection::State>().confirmed(&sessions, document);
                let selection = confirmed.as_ref().map(|(generation, identity)| (*generation, identity));
                state.record_key(document, &id, key, selection, now);
            }
        }
        sessions.input(webview.label(), &id, &bytes)
    }).await
}

#[tauri::command]
pub(crate) async fn terminal_resize(webview: WebviewWindow, id: String, rows: u16, cols: u16) -> Result<(), String> {
    blocking(move || webview.state::<Sessions>().resize(webview.label(), &id, size(rows, cols))).await
}

#[tauri::command]
pub(crate) fn terminal_ack(webview: WebviewWindow, id: String, position: u64) -> Result<(), String> {
    webview.state::<Sessions>().ack(webview.label(), &id, position)
}

#[tauri::command]
pub(crate) async fn terminal_close(webview: WebviewWindow, id: String) -> Result<(), String> {
    let document = webview.state::<Sessions>().document(webview.label());
    blocking(move || {
        let result = webview.state::<Sessions>().close(webview.label(), &id)?;
        webview.state::<crate::editor::sftp_ipc::SftpIpc>().close_parent(webview.label(), document, &id);
        result.warning().map_or(Ok(()), Err)
    }).await
}

/// 사용자가 명시적으로 요청한 승격 실행 한 건이다. 일반 PTY 출력이나 AI 제안은 이 명령을 부르지 않는다.
#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
pub(crate) fn elevation_open(invoke: Invoke) {
    const COMMAND: &str = "elevation_open";
    let Invoke { message, resolver, acl } = invoke;
    let caller = message.webview_ref();
    let document = caller.state::<Sessions>().document(caller.label());
    let argument = |key| CommandItem { name: COMMAND, key, plugin: None, message: &message, acl: &acl };
    let parse = || {
        Ok((
            WebviewWindow::from_command(argument("webview"))?,
            u16::from_command(argument("rows"))?,
            u16::from_command(argument("cols"))?,
            Channel::<InvokeResponseBody>::from_command(argument("output"))?,
            String::from_command(argument("program"))?,
            Vec::<String>::from_command(argument("args"))?,
            String::from_command(argument("item"))?,
        ))
    };
    let (webview, rows, cols, output, program, args, item) = match parse() {
        Ok(args) => args,
        Err(error) => { resolver.invoke_error(error); return; }
    };
    if caller.label() != "main" || caller.window_ref().label() != "main" || acl.is_none() {
        resolver.reject("앱의 로컬 기본 창에서만 sudo 승격을 요청할 수 있습니다");
        return;
    }
    resolver.respond_async(async move {
        blocking(move || {
            let sessions = webview.state::<Sessions>();
            let owner = webview.label();
            let size = size(rows, cols);
            let send = move |event: super::Event| event.send(&output).map_err(|error| error.to_string());
            let program = crate::elevation::canonical_program(&program)
                .ok_or("실행 파일은 존재하는 절대 경로여야 합니다")?;
            let caller_account = crate::elevation::account()
                .ok_or("호출 계정을 확인할 수 없습니다")?;
            let request = crate::elevation::Request::new(caller_account, program, args, item);
            let request_id = crate::elevation::request_id()
                .ok_or("승격 요청 ID를 만들 수 없습니다")?;
            let target = crate::elevation::Document { owner: owner.into(), document, request: request_id };
            crate::elevation::validate_request(&request)
                .map_err(|_| "승격 요청이 올바르지 않습니다")?;
            let (helper, sudo) = crate::elevation::binaries().map_err(|_| {
                "sudo 승격 헬퍼를 확인할 수 없습니다. 터미널에서 직접 입력하세요"
            })?;
            let vault = crate::local_auth::Bitwarden::discover().map_err(|_| {
                "Bitwarden CLI를 확인할 수 없습니다. 터미널에서 직접 입력하세요"
            })?;
            tauri::async_runtime::block_on(vault.ready()).map_err(|_| {
                "Bitwarden 금고를 사용할 수 없습니다. 터미널에서 직접 입력하세요"
            })?;
            let message = crate::elevation::message(&target, &request);
            if !crate::ssh::registration::confirm_elevation(&webview.as_ref().window(), message)? {
                return Err("sudo 승격 승인이 취소되었습니다".into());
            }
            // 승인 창을 읽는 시간은 30초 승인 예산에 넣지 않는다. 창을 닫은 뒤 한 번만 소비한다.
            let pending = crate::elevation::Pending::request(target.clone(), request.clone())
                .map_err(|error| crate::elevation::failure(error).unwrap_or("승격 요청이 올바르지 않습니다"))?;
            let approval = pending.approve(&target, &request)
                .map_err(|error| crate::elevation::failure(error).unwrap_or("승격 승인이 만료되었습니다"))?;
            let launch = crate::elevation::Launch::new(approval, target, request, helper, sudo, vault);
            sessions.open_document(owner, document, size, Launch::Elevated(launch), send)
        }).await.map_err(Into::into)
    });
}

/// F01에서 검증한 같은 서버·계정의 별도 SSH 채널로 원격 승격을 요청한다.
#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
pub(crate) fn elevation_open_remote(invoke: Invoke) {
    const COMMAND: &str = "elevation_open_remote";
    let Invoke { message, resolver, acl } = invoke;
    let caller = message.webview_ref();
    let document = caller.state::<Sessions>().document(caller.label());
    let argument = |key| CommandItem { name: COMMAND, key, plugin: None, message: &message, acl: &acl };
    let parse = || {
        Ok((
            WebviewWindow::from_command(argument("webview"))?,
            u16::from_command(argument("rows"))?,
            u16::from_command(argument("cols"))?,
            Channel::<InvokeResponseBody>::from_command(argument("output"))?,
            String::from_command(argument("targetId"))?,
            String::from_command(argument("program"))?,
            Vec::<String>::from_command(argument("args"))?,
            String::from_command(argument("item"))?,
        ))
    };
    let (webview, rows, cols, output, target_id, program, args, item) = match parse() {
        Ok(args) => args,
        Err(error) => { resolver.invoke_error(error); return; }
    };
    if caller.label() != "main" || caller.window_ref().label() != "main" || acl.is_none() {
        resolver.reject("앱의 로컬 기본 창에서만 원격 sudo 승격을 요청할 수 있습니다");
        return;
    }
    resolver.respond_async(async move {
        blocking(move || {
            let sessions = webview.state::<Sessions>();
            let owner = webview.label();
            let size = size(rows, cols);
            let send = move |event: super::Event| event.send(&output).map_err(|error| error.to_string());
            let directory = webview.path().app_config_dir().map_err(|error| stage("앱 설정 디렉터리", error))?;
            let original = crate::connections::with_saved_ssh(&directory, &target_id, |saved| saved)?
                .ok_or("저장된 SSH 연결을 찾을 수 없습니다")?;
            let (revision, ssh) = original;
            let plan = crate::ssh::terminal::remote_plan(&directory, &ssh)?;
            crate::elevation::remote::verify(&plan, &ssh.account)?;
            let program = crate::elevation::remote_program(&program)
                .ok_or("원격 실행 파일은 서버의 POSIX 절대 경로여야 합니다")?;
            let target = format!("{}@{}:{}", ssh.account, ssh.host, ssh.port);
            let request = crate::elevation::Request::new_remote(
                target,
                ssh.account.clone(),
                program,
                args,
                item,
            );
            let request_id = crate::elevation::request_id()
                .ok_or("원격 승격 요청 ID를 만들 수 없습니다")?;
            let approval_document = crate::elevation::Document {
                owner: owner.into(),
                document,
                request: request_id,
            };
            crate::elevation::validate_request(&request)
                .map_err(|_| "원격 승격 요청이 올바르지 않습니다")?;
            let vault = crate::local_auth::Bitwarden::discover().map_err(|_| {
                "Bitwarden CLI를 확인할 수 없습니다. 터미널에서 직접 입력하세요"
            })?;
            tauri::async_runtime::block_on(vault.ready()).map_err(|_| {
                "Bitwarden 금고를 사용할 수 없습니다. 터미널에서 직접 입력하세요"
            })?;
            let message = crate::elevation::message(&approval_document, &request);
            if !crate::ssh::registration::confirm_elevation(&webview.as_ref().window(), message)? {
                return Err("원격 sudo 승격 승인이 취소되었습니다".into());
            }
            let current = crate::connections::with_saved_ssh(&directory, &target_id, |saved| saved)?;
            if current.as_ref() != Some(&(revision, ssh)) {
                return Err("저장된 SSH 연결이 바뀌었습니다".into());
            }
            let pending = crate::elevation::Pending::request(approval_document.clone(), request.clone())
                .map_err(|error| crate::elevation::failure(error).unwrap_or("원격 승격 요청이 올바르지 않습니다"))?;
            let _approval = pending.approve(&approval_document, &request)
                .map_err(|error| crate::elevation::failure(error).unwrap_or("승격 승인이 만료되었습니다"))?;
            let launch = crate::elevation::remote::Launch::build(plan, &approval_document, &request, vault)?;
            sessions.open_document(owner, document, size, Launch::RemoteElevated(launch), send)
        }).await.map_err(Into::into)
    });
}
