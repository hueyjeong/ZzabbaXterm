use std::{
    io::{Read, Write},
    path::PathBuf,
    process::Command,
    sync::mpsc,
    time::{Duration, Instant},
};

use portable_pty::{PtySize, native_pty_system};

use super::*;
use crate::ssh::{Endpoint, session_files};

const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";
const SHA: &str = "SHA256:UCUiLr7Pjs9wFFJMDByLgc3NrtdU344OgUM45wZPcIQ";

#[test]
fn invalid_confirmation_creates_no_launch_files() {
    let root = session_files(&std::env::temp_dir()).unwrap();
    let store = TrustStore::open(root.path()).unwrap();
    let endpoint = Endpoint::new("dev.example.invalid", 2222).unwrap();
    let other_port = Endpoint::new("dev.example.invalid", 22).unwrap();
    assert!(matches!(ApprovedHost::confirm(&endpoint, &other_port, SHA), Err(Error::ApprovedEndpointMismatch)));
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, SHA).unwrap();
    store.register(&approved, KEY).unwrap();
    for (account, approved_account, auth, expected) in [
        ("wrong-user", "approved-user", Authentication::ManualPassword, Error::ApprovedAccountMismatch),
        ("approved-user", "approved-user", Authentication::StoredPassword { item: "21eb36c3-234d-439d-9e1f-1d3cc5ba8125" }, Error::StoredAuthenticationUnavailable),
    ] {
        let actual = match DirectLaunch::prepare(&store, &approved, account, approved_account, auth, Transport::Direct) {
            Err(error) => error,
            Ok(_) => panic!("승인하지 않은 기동 계획"),
        };
        assert_eq!(std::mem::discriminant(&actual), std::mem::discriminant(&expected));
        assert!(std::fs::read_dir(root.path()).unwrap().all(|item| item.unwrap().file_type().unwrap().is_file()));
    }
}

#[test]
fn direct_ssh_fixture_receives_exact_plan_and_pty_input() {
    let root = session_files(&std::env::temp_dir()).unwrap();
    let store = TrustStore::open(root.path()).unwrap();
    let endpoint = Endpoint::new("dev.example.invalid", 2222).unwrap();
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, SHA).unwrap();
    store.register(&approved, KEY).unwrap();

    let fixture_root = tempfile::tempdir().unwrap();
    let fixture = fixture_root.path().join(format!("ssh fixture{}", std::env::consts::EXE_SUFFIX));
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ssh/terminal/fixture.rs");
    let compiler = option_env!("RUSTC").unwrap_or("rustc");
    let compiled = Command::new(compiler).arg(source).arg("-o").arg(&fixture).output().unwrap();
    assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));

    for close in [false, true] {
        let launch = DirectLaunch::prepare(&store, &approved, "approved-user", "approved-user", Authentication::Agent { public_key: KEY }, Transport::Direct).unwrap();
        let (mut command, plan) = launch.into_parts();
        let invocation = plan.invocation(Client::Ssh).unwrap();
        let config = PathBuf::from(invocation.args[1]);
        // 시험에서만 실행 파일을 교체하며 앱이 만든 인자와 환경은 그대로 둔다.
        command.get_argv_mut()[0] = fixture.as_os_str().to_owned();
        if close {
            command.env("ZZABBA_F01_FIXTURE_WAIT", "1");
        } else {
            command.env_remove("ZZABBA_F01_FIXTURE_WAIT");
        }
        let pair = native_pty_system().openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }).unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let (sender, receiver) = mpsc::channel();
        let drain = std::thread::spawn(move || {
            let mut bytes = [0; 1024];
            while let Ok(1..) = reader.read(&mut bytes).map(|count| {
                let _ = sender.send(bytes[..count].to_vec());
                count
            }) {}
        });
        let mut output = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut until = |marker: &[u8]| -> bool {
            while !output.windows(marker.len()).any(|bytes| bytes == marker) {
                match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(bytes) => output.extend(bytes),
                    Err(_) => return false,
                }
            }
            true
        };
        let observed = (|| -> std::io::Result<bool> {
            #[cfg(windows)]
            if until(b"\x1b[6n") {
                writer.write_all(b"\x1b[1;1R")?;
            } else {
                return Ok(false);
            }
            if !until(b"F01-READY") {
                return Ok(false);
            }
            writer.write_all(b"fixture-input\r")?;
            Ok(until(b"F01-INPUT-OK"))
        })();
        let files_survived = config.is_file() && config.with_file_name("known_hosts").is_file();
        if close || !matches!(observed, Ok(true)) {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        drop(writer);
        drop(pair.master);
        drain.join().unwrap();
        drop(plan);

        assert!(observed.unwrap(), "{}", String::from_utf8_lossy(&output));
        assert!(files_survived);
        if !close {
            assert_eq!(status.exit_code(), 7);
        }
        assert!(!config.parent().unwrap().exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1, "영구 핀만 남아야 합니다");
    }
}

#[test]
fn remote_editor_approval_shows_the_ssm_transport() {
    let directory = tempfile::tempdir().unwrap();
    let store = TrustStore::create(&directory.path().join(crate::ssh::TRUST_DIRECTORY)).unwrap();
    let endpoint = Endpoint::new("dev.example.invalid", 2222).unwrap();
    store.register(&ApprovedHost::confirm(&endpoint, &endpoint, SHA).unwrap(), KEY).unwrap();
    let ssh = serde_json::from_value::<Ssh>(serde_json::json!({
        "host": "dev.example.invalid", "port": 2222, "account": "deploy", "hostKeyFingerprint": SHA,
        "transport": {"kind": "ssm", "profile": "dev-sso", "region": "ap-northeast-2", "instanceId": "i-0123456789abcdef0"},
        "auth": {"kind": "agent", "publicKeyPath": directory.path().join("agent.pub")},
    })).unwrap();
    // SSM 대상은 승인 창을 띄우기 전에 전달을 열지 않는다.
    let (launch, message) = prepare_remote_editor(directory.path(), "profile-1", std::num::NonZeroU64::MIN, &ssh).unwrap();
    assert!(matches!(launch, SavedLaunch::Forward(_)));
    assert!(message.contains("\n전송: SSM · dev-sso · ap-northeast-2 · i-0123456789abcdef0\n"), "{message}");
}

#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
mod stored {
    use std::{
        path::Path,
        process::Command,
        time::Duration,
    };

    use serde_json::json;

    use crate::{
        connections::profile::Ssh,
        local_auth::{Bitwarden, Error},
        ssh::{ApprovedHost, Endpoint, TRUST_DIRECTORY, TrustStore, tests::FakeForward},
    };

    use super::super::{APPROVAL_TTL, DirectLaunch, SavedLaunch, prepare_stored_bounded, prepare_stored_with, saved, transport};
    use tokio_util::sync::CancellationToken;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";
    const SHA: &str = "SHA256:UCUiLr7Pjs9wFFJMDByLgc3NrtdU344OgUM45wZPcIQ";
    const ITEM: &str = "21eb36c3-234d-439d-9e1f-1d3cc5ba8125";

    fn target() -> (tempfile::TempDir, Ssh, Endpoint) {
        let directory = tempfile::tempdir().unwrap();
        let store = TrustStore::create(&directory.path().join(TRUST_DIRECTORY)).unwrap();
        let endpoint = Endpoint::new("dev.example.invalid", 2222).unwrap();
        store
            .register(&ApprovedHost::confirm(&endpoint, &endpoint, SHA).unwrap(), KEY)
            .unwrap();
        let ssh = serde_json::from_value::<Ssh>(json!({
            "host": "dev.example.invalid",
            "port": 2222,
            "account": "deploy",
            "hostKeyFingerprint": SHA,
            "transport": {"kind": "direct"},
            "auth": {"kind": "storedPassword", "itemId": ITEM},
        }))
        .unwrap();
        (directory, ssh, endpoint)
    }

    fn ready(launch: SavedLaunch) -> DirectLaunch {
        match launch {
            SavedLaunch::Ready(launch) => launch,
            SavedLaunch::Forward(_) => panic!("직접 SSH 저장 암호는 바로 실행을 준비합니다"),
        }
    }

    fn vault(directory: &Path) -> Bitwarden {
        let path = directory.join(if cfg!(windows) { "bw.exe" } else { "bw" });
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/auth/tests/bw_fixture.rs");
        let built = Command::new(option_env!("RUSTC").unwrap_or("rustc"))
            .arg(source)
            .arg("-o")
            .arg(&path)
            .output()
            .unwrap();
        assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
        Bitwarden::fixture_at(&path, "isolated-fixture-session")
    }

    #[test]
    fn stored_password_needs_confirmation_and_shows_target() {
        let (directory, ssh, endpoint) = target();
        let vault = vault(directory.path());
        let mut shown = String::new();
        let result = prepare_stored_with(directory.path(), &ssh, &endpoint, ITEM, "main", 1, Ok(vault), |message| {
            shown = message;
            Ok(false)
        });
        assert!(result.is_err());
        assert_eq!(result.err().unwrap(), "저장된 SSH 암호 승인이 취소되었습니다");
        assert!(shown.contains(ITEM), "{shown}");
        assert!(shown.contains("deploy") && shown.contains(SHA), "{shown}");
        assert!(shown.contains("[dev.example.invalid]:2222"), "{shown}");
        assert!(!shown.contains("전송:"), "{shown}");
    }

    #[test]
    fn stored_password_over_ssm_confirms_before_opening_the_forward() {
        let (directory, mut ssh, endpoint) = target();
        ssh.transport = serde_json::from_value(json!({
            "kind": "ssm", "profile": "dev-sso", "region": "ap-northeast-2", "instanceId": "i-0123456789abcdef0",
        })).unwrap();
        for approve in [false, true] {
            let vault = vault(directory.path());
            let mut shown = String::new();
            let result = prepare_stored_with(directory.path(), &ssh, &endpoint, ITEM, "main", 1, Ok(vault), |message| {
                shown = message;
                Ok(approve)
            });
            for field in [ITEM, "deploy", SHA, "[dev.example.invalid]:2222", "\n전송: SSM · dev-sso · ap-northeast-2 · i-0123456789abcdef0\n"] {
                assert!(shown.contains(field), "{shown}");
            }
            // 승인 전에는 전달을 열지 않고, 승인 뒤에도 탭이 토큰으로 준비하도록 미룬다.
            match result {
                Err(error) => assert!(!approve && error == "저장된 SSH 암호 승인이 취소되었습니다", "{error}"),
                Ok(launch) => assert!(approve && matches!(launch, SavedLaunch::Forward(_))),
            }
        }
    }

    fn approved_over_ssm(budget: Duration, open: super::super::OpenTransport) -> (tempfile::TempDir, Result<DirectLaunch, String>) {
        let (directory, mut ssh, endpoint) = target();
        ssh.transport = serde_json::from_value(json!({
            "kind": "ssm", "profile": "dev-sso", "region": "ap-northeast-2", "instanceId": "i-0123456789abcdef0",
        })).unwrap();
        let vault = vault(directory.path());
        let launch = prepare_stored_bounded(directory.path(), &ssh, &endpoint, ITEM, "main", 1, budget, open, Ok(vault), |_| Ok(true));
        let Ok(SavedLaunch::Forward(forward)) = launch else { panic!("SSM 저장 암호는 탭에서 전달을 준비합니다") };
        let launch = forward.prepare(&CancellationToken::new());
        (directory, launch)
    }

    #[test]
    fn stored_password_over_ssm_approves_after_the_forward() {
        let (_directory, launch) = approved_over_ssm(APPROVAL_TTL, |_, _, _, _| Ok(FakeForward::transport(40022).0));
        let mut launch = launch.ok().unwrap();
        let stored = launch.plan.stored().unwrap();
        assert_eq!([stored.target.as_str(), stored.user.as_str(), stored.item.as_str()], ["[dev.example.invalid]:2222", "deploy", ITEM]);
        // Pending::request와 approve가 성공해야 승인이 실행에 묶인다.
        assert!(launch.stored.take().is_some());
    }

    #[test]
    fn stored_password_over_ssm_expires_when_the_forward_outlasts_the_approval() {
        let (_directory, result) = approved_over_ssm(Duration::from_millis(50), |_, _, _, _| {
            std::thread::sleep(Duration::from_millis(120));
            Ok(FakeForward::transport(40022).0)
        });
        assert_eq!(result.err().unwrap(), "저장된 SSH 암호 승인이 만료되었습니다");
    }

    #[test]
    fn manual_fallback_over_ssm_defers_the_forward() {
        let (directory, mut ssh, endpoint) = target();
        ssh.transport = serde_json::from_value(json!({
            "kind": "ssm", "profile": "dev-sso", "region": "ap-northeast-2", "instanceId": "i-0123456789abcdef0",
        })).unwrap();
        let launch = prepare_stored_with(directory.path(), &ssh, &endpoint, ITEM, "main", 1, Err(Error::VaultUnavailable), |_| {
            panic!("금고 없이 승인 창을 열었습니다")
        });
        assert!(matches!(launch, Ok(SavedLaunch::Forward(_))));
    }

    #[test]
    fn locked_vault_falls_back_to_manual_input() {
        let (directory, ssh, endpoint) = target();
        let vault = vault(directory.path());
        std::fs::write(directory.path().join("mode"), "locked").unwrap();
        let launch = ready(prepare_stored_with(directory.path(), &ssh, &endpoint, ITEM, "main", 1, Ok(vault), |_| {
            panic!("잠긴 금고에서 승인 창을 열었습니다")
        })
        .ok()
        .unwrap());
        assert!(launch.plan.stored().is_none());
        assert!(launch.notice().unwrap().contains("Bitwarden 금고가 잠겨 있습니다"));
        assert_eq!(launch.command.get_env("SSH_ASKPASS"), None);
    }

    #[test]
    fn missing_vault_falls_back_with_reason() {
        let (directory, ssh, endpoint) = target();
        let launch = ready(prepare_stored_with(directory.path(), &ssh, &endpoint, ITEM, "main", 1, Err(Error::VaultUnavailable), |_| {
            panic!("금고 없이 승인 창을 열었습니다")
        })
        .ok()
        .unwrap());
        assert!(launch.notice().unwrap().contains("Bitwarden CLI를 찾을 수 없습니다"));
    }

    #[test]
    fn stored_approval_is_consumed_once() {
        let (directory, ssh, endpoint) = target();
        let vault = vault(directory.path());
        let mut launch = ready(prepare_stored_with(directory.path(), &ssh, &endpoint, ITEM, "main", 1, Ok(vault), |_| Ok(true))
            .ok()
            .unwrap());
        assert!(launch.stored.take().is_some());
        assert!(launch.stored.take().is_none());
    }

    #[test]
    fn noninteractive_paths_still_refuse_stored_password() {
        let (directory, ssh, _) = target();
        assert_eq!(saved(directory.path(), &ssh, &CancellationToken::new()).err().unwrap(), "저장된 SSH 암호를 사용할 수 없습니다");
    }

    #[test]
    fn approval_window_outlasts_the_old_thirty_second_basis() {
        assert!(APPROVAL_TTL >= Duration::from_secs(120));
        let (directory, ssh, endpoint) = target();
        let vault = vault(directory.path());
        // 창을 읽는 시간은 승인 기한에서 따로 재고, 승인 뒤 교환 예산은 spawn에서 새로 잰다.
        let launch = prepare_stored_bounded(
            directory.path(), &ssh, &endpoint, ITEM, "main", 1, Duration::from_secs(5), transport, Ok(vault),
            |_| {
                std::thread::sleep(Duration::from_millis(60));
                Ok(true)
            },
        ).ok().unwrap();
        let mut launch = ready(launch);
        assert!(launch.stored.take().is_some());
    }

    #[test]
    fn approval_window_expiry_is_separate_from_the_exchange_budget() {
        let (directory, ssh, endpoint) = target();
        let vault = vault(directory.path());
        let result = prepare_stored_bounded(
            directory.path(), &ssh, &endpoint, ITEM, "main", 1, Duration::from_millis(10), transport, Ok(vault),
            |_| {
                std::thread::sleep(Duration::from_millis(30));
                Ok(true)
            },
        ).map(|_| ());
        assert_eq!(result.err().unwrap(), "저장된 SSH 암호 승인이 만료되었습니다");
    }
}
