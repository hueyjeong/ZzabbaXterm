use std::path::Path;

use portable_pty::CommandBuilder;

use crate::{connections::{profile::{MethodRef, Shell}, with_saved_method, with_saved_ssh}, ssh::terminal::{SavedLaunch, prepare_remote_editor}};

use super::{process::Launch, remote};
use crate::ssm::CancellationToken;

pub(super) fn launch(config: &Path, id: &str) -> Result<Launch, String> {
    with_saved_method(config, id, |method, _| match method {
        MethodRef::Local { shell, start_directory } => local(shell, start_directory).map(Launch::Local),
        MethodRef::Ssm { profile, region, instance_id } => remote::Target::new(profile, region, instance_id).map(Launch::Ssm),
        MethodRef::Ssh { ssh } => crate::ssh::terminal::saved(config, ssh, &CancellationToken::new()).map(Launch::Ssh),
        MethodRef::S3 { .. } => Err("S3 연결은 스토리지 탭에서 열 수 있습니다".into()),
        MethodRef::Desktop { .. } => Err("원격 화면 연결은 RDP·VNC 탭에서 열 수 있습니다".into()),
    })
}

/// 터미널 탭에는 저장 암호 승인 경로를 연다. 연결 시험·터널은 계속 `launch`를 쓴다.
pub(super) fn launch_interactive(
    config: &Path,
    id: &str,
    owner: &str,
    document: u64,
    confirm: impl FnOnce(String) -> Result<bool, String>,
) -> Result<Launch, String> {
    with_saved_method(config, id, |method, _| match method {
        MethodRef::Local { shell, start_directory } => local(shell, start_directory).map(Launch::Local),
        MethodRef::Ssm { profile, region, instance_id } => remote::Target::new(profile, region, instance_id).map(Launch::Ssm),
        MethodRef::Ssh { ssh } => crate::ssh::terminal::saved_interactive(config, ssh, owner, document, confirm).map(Launch::from),
        MethodRef::S3 { .. } => Err("S3 연결은 스토리지 탭에서 열 수 있습니다".into()),
        MethodRef::Desktop { .. } => Err("원격 화면 연결은 RDP·VNC 탭에서 열 수 있습니다".into()),
    })
}

pub(super) fn approve_remote_editor<T>(config: &Path, id: &str,
    confirm: impl FnOnce(String) -> Result<bool, String>,
    register: impl FnOnce(SavedLaunch) -> Result<T, String>,
) -> Result<T, String> {
    let saved = with_saved_ssh(config, id, |saved| saved)?.ok_or("저장된 SSH 연결을 찾을 수 없습니다")?;
    let (launch, message) = prepare_remote_editor(config, id, saved.0, &saved.1)?;
    if !confirm(message)? {
        return Err("SSH nano 연동 승인이 취소되었습니다".into());
    }
    // 프로필과 현재 문서가 모두 일치하는 동안 세션만 등록한다.
    with_saved_ssh(config, id, |current| {
        if current.as_ref() != Some(&saved) {
            return Err("저장된 SSH 연결이 바뀌었습니다".into());
        }
        register(launch)
    })?
}

#[cfg(test)]
#[test]
fn nano_approval_admits_only_the_confirmed_saved_ssh_and_document() {
    use crate::{connections::{profile::Draft, save_test_profile, tests::{TestDir, ssh_draft}},
        ssh::{ApprovedHost, Client, Endpoint, TRUST_DIRECTORY, TrustStore}};
    use serde_json::{Value, json};
    use std::{cell::Cell, fs};

    const ID: &str = "ssh-server";
    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";
    const SHA: &str = "SHA256:UCUiLr7Pjs9wFFJMDByLgc3NrtdU344OgUM45wZPcIQ";
    let config = TestDir::new();
    let directory = config.path();
    let trust = TrustStore::create(&directory.join(TRUST_DIRECTORY)).unwrap();
    let endpoint = Endpoint::new("dev.example.test", 2222).unwrap();
    trust.register(&ApprovedHost::confirm(&endpoint, &endpoint, SHA).unwrap(), KEY).unwrap();
    let mut draft = ssh_draft(directory);
    let public_key = directory.join("agent.pub");
    fs::write(&public_key, KEY).unwrap();
    draft["target"]["ssh"]["auth"] = json!({"kind": "agent", "publicKeyPath": public_key});
    draft["target"]["ssh"]["host"] = json!("DEV.example.test");
    draft["target"]["ssh"]["hostKeyFingerprint"] = json!(SHA);
    save_test_profile(directory, serde_json::from_value::<Draft>(draft).unwrap().validate().unwrap());
    let file = directory.join("connections.json");
    let original = fs::read(&file).unwrap();

    let mut wrong_pin: Value = serde_json::from_slice(&original).unwrap();
    wrong_pin["targets"][0]["profile"]["ssh"]["hostKeyFingerprint"] =
        json!("SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    fs::write(&file, serde_json::to_vec(&wrong_pin).unwrap()).unwrap();
    assert_eq!(approve_remote_editor(directory, ID, |_| panic!("PIN 불일치 상태에서 승인 창을 열었습니다"),
        |_| -> Result<(), String> { panic!("PIN 불일치 상태에서 세션을 등록했습니다") },
    ).unwrap_err(), "등록된 SSH 호스트 키의 지문이 다릅니다");

    for (change, expected) in [
        ("cancel", Some("SSH nano 연동 승인이 취소되었습니다")),
        ("revision", Some("저장된 SSH 연결이 바뀌었습니다")),
        ("host", Some("저장된 SSH 연결이 바뀌었습니다")),
        ("port", Some("저장된 SSH 연결이 바뀌었습니다")),
        ("account", Some("저장된 SSH 연결이 바뀌었습니다")),
        ("hostKeyFingerprint", Some("저장된 SSH 연결이 바뀌었습니다")),
        ("auth", Some("저장된 SSH 연결이 바뀌었습니다")),
        ("navigate", Some("open: 문서가 바뀌었습니다")),
        ("close", Some("open: 창이 닫혔습니다")),
        ("approve", None),
    ] {
        fs::write(&file, &original).unwrap();
        let sessions = super::super::Sessions::default();
        let document = sessions.document("main");
        let confirmations = Cell::new(0);
        let launch_admissions = Cell::new(0);
        let result = approve_remote_editor(directory, ID, |message| {
            confirmations.set(confirmations.get() + 1);
            for field in ["프로필: ssh-server", "리비전: 1", "서버: dev.example.test", "포트: 2222", "계정: deploy", SHA,
                "관리자가 SSH 셸과 SFTP에서 같은 경로가 같은 파일을 가리킨다고 확인했습니까?"] {
                assert!(message.contains(field), "{message}");
            }
            let mut current: Value = serde_json::from_slice(&original).unwrap();
            let entry = &mut current["targets"][0];
            match change {
                "cancel" => return Ok(false),
                "navigate" => sessions.navigate("main"),
                "close" => assert!(sessions.close_ssh_probes(Some("main"))),
                "approve" => {
                    // 준비가 이미 끝났으므로 승인 뒤에는 Agent 공개 키를 다시 읽지 않는다.
                    fs::remove_file(&public_key).unwrap();
                }
                "revision" => entry["revision"] = json!(2),
                field => entry["profile"]["ssh"][field] = match field {
                    "host" => json!("other.example.test"),
                    "port" => json!(2022),
                    "account" => json!("other"),
                    "hostKeyFingerprint" => json!("SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
                    "auth" => json!({"kind": "manualPassword"}),
                    _ => unreachable!(),
                },
            }
            fs::write(&file, serde_json::to_vec(&current).unwrap()).unwrap();
            Ok(true)
        }, |launch| {
            let command = Launch::from(launch);
            let (id, session) = sessions.register_document("main", document, &command, true)?;
            let registered = sessions.registry.lock().unwrap().sessions.remove(&id).unwrap();
            assert!(std::sync::Arc::ptr_eq(&registered, &session));
            launch_admissions.set(launch_admissions.get() + 1);
            let Launch::Ssh(launch) = command else { unreachable!() };
            let (_, plan) = launch.into_parts();
            let shell = plan.invocation(Client::Ssh).unwrap();
            let sftp = plan.invocation(Client::Sftp).unwrap();
            assert_eq!(shell.args[1], sftp.args[4]);
            let config = fs::read_to_string(Path::new(shell.args[1])).unwrap();
            assert!(config.contains("HostName dev.example.test\nPort 2222\nUser deploy\n"));
            Ok(())
        });
        assert_eq!(result.err().as_deref(), expected, "{change}");
        assert_eq!(confirmations.get(), 1, "{change}");
        assert_eq!(launch_admissions.get(), u8::from(expected.is_none()), "{change}");
        assert!(sessions.registry.lock().unwrap().sessions.is_empty(), "{change}");
    }
}

#[cfg(test)]
#[test]
fn rejects_saved_s3_without_launching_a_pty() {
    use crate::connections::{profile::Draft, save_test_profile, tests::{TestDir, s3_draft}};

    let config = TestDir::new();
    let draft = serde_json::from_value::<Draft>(s3_draft(config.path())).unwrap().validate().unwrap();
    save_test_profile(config.path(), draft);
    assert_eq!(launch(config.path(), "s3-bucket").err().as_deref(), Some("S3 연결은 스토리지 탭에서 열 수 있습니다"));
}

#[cfg(test)]
pub(super) fn command(config: &Path, id: &str) -> Result<CommandBuilder, String> {
    match launch(config, id)? {
        Launch::Local(command) => Ok(command),
        Launch::Ssm(_) | Launch::Ssh(_) | Launch::Forward(_) | Launch::AiControl(..) => Err("로컬 연결이 아닙니다".into()),
        #[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
        Launch::Elevated(_) | Launch::RemoteElevated(_) => Err("로컬 연결이 아닙니다".into()),
    }
}

fn local(shell: Shell, directory: &Path) -> Result<CommandBuilder, String> {
    super::validate_working_directory(directory)?;

    // portable-pty는 시작 직전에 경로가 사라지면 홈을 선택할 수 있다. 프롬프트 전에 실제 위치를 확인한다.
    let mut command = match shell {
        #[cfg(windows)]
        Shell::PowerShell => {
            let path = std::env::var_os("PATH").ok_or("PowerShell: PATH가 없습니다")?;
            let executable = std::env::split_paths(&path).filter(|path| path.is_absolute())
                .map(|path| path.join("pwsh.exe")).find(|path| path.is_file())
                .ok_or("PowerShell: pwsh.exe를 찾을 수 없습니다")?;
            let mut command = CommandBuilder::new(executable);
            command.args(["-NoLogo", "-NoProfile", "-NoExit", "-Command"]);
            command.arg(format!(r#"
                $directory = $env:ZZABBA_C01_CWD
                Remove-Item Env:ZZABBA_C01_CWD
                try {{
                    if (({}) -and [IO.Directory]::Exists($directory)) {{ return }}
                }} catch {{ [Console]::Error.WriteLine($_.Exception.Message) }}
                [Console]::Error.WriteLine('Could not start in the saved working directory.')
                exit 23
            "#, super::POWERSHELL_CWD_MATCH));
            command
        }
        #[cfg(unix)]
        Shell::Bash => {
            let mut command = CommandBuilder::new("/bin/bash");
            command.env_remove("BASH_ENV");
            command.args(["--noprofile", "--norc", "-c", r#"
                if [[ ! . -ef "$ZZABBA_C01_CWD" ]]; then
                    builtin printf '%s\n' 'Could not start in the saved working directory.' >&2
                    exit 23
                fi
                unset ZZABBA_C01_CWD
                exec /bin/bash --noprofile --norc -i
            "#]);
            command
        }
        _ => return Err("지원하지 않는 셸입니다".into()),
    };
    command.cwd(directory);
    command.env("ZZABBA_C01_CWD", directory);
    command.env("TERM", "xterm-256color");
    Ok(command)
}
