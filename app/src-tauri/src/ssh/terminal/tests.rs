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
        ("approved-user", "approved-user", Authentication::StoredPassword, Error::StoredAuthenticationUnavailable),
    ] {
        let actual = match DirectLaunch::prepare(&store, &approved, account, approved_account, auth) {
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
        let launch = DirectLaunch::prepare(&store, &approved, "approved-user", "approved-user", Authentication::Agent { public_key: KEY }).unwrap();
        let (mut command, plan) = launch.into_parts();
        let invocation = plan.invocation(Client::Ssh);
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
