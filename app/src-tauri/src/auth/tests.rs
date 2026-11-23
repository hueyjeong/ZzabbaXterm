use std::{
    ffi::OsStr,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

use portable_pty::{MasterPty, PtySize, native_pty_system};
use zeroize::Zeroizing;

use super::*;

const ITEM: &str = "21eb36c3-234d-439d-9e1f-1d3cc5ba8125";

fn document() -> Document {
    Document {
        owner: "fixture-window".into(),
        document: 1,
        tab: "fixture-tab".into(),
    }
}

const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";

fn plan(dir: &Path, port: u16, user: &str, key: &str, item: &str) -> Arc<ConnectionPlan> {
    use crate::ssh::{ApprovedHost, Authentication, Endpoint, Transport, TrustStore};
    let store = if dir.exists() {
        TrustStore::open(dir)
    } else {
        TrustStore::create(dir)
    }
    .unwrap();
    let endpoint = Endpoint::new("127.0.0.1", port).unwrap();
    let fingerprint = ssh_key::PublicKey::from_openssh(key)
        .unwrap()
        .fingerprint(ssh_key::HashAlg::Sha256)
        .to_string();
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, &fingerprint).unwrap();
    store.register(&approved, key).unwrap();
    Arc::new(
        ConnectionPlan::prepare(
            &store,
            &approved,
            user,
            Transport::Direct,
            Authentication::StoredPassword { item },
        )
        .unwrap(),
    )
}

#[test]
fn approval_is_bound_to_document_target_user_and_item() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("trust");
    let original = document();
    let approved = plan(&dir, 2222, "fixture-user", KEY, ITEM);
    let pending = || Pending::request(original.clone(), approved.clone()).unwrap();
    assert_eq!(pending().details().target, "[127.0.0.1]:2222");
    assert_eq!(pending().details().user, "fixture-user");
    assert_eq!(pending().details().item, ITEM);
    assert!(pending().details().fingerprint.starts_with("SHA256:"));
    for changed in [
        Document {
            owner: "other".into(),
            ..original.clone()
        },
        Document {
            document: 2,
            ..original.clone()
        },
        Document {
            tab: "other".into(),
            ..original.clone()
        },
    ] {
        assert!(matches!(
            pending().approve(&changed, &approved),
            Err(Error::InvalidApproval)
        ));
    }
    for alternative in [
        plan(&dir, 22, "fixture-user", KEY, ITEM),
        plan(&dir, 2222, "other-user", KEY, ITEM),
        plan(&dir, 2222, "fixture-user", KEY, ITEM),
    ] {
        assert!(matches!(
            pending().approve(&original, &alternative),
            Err(Error::InvalidApproval)
        ));
    }
    let mut expired = pending();
    expired.expires = Instant::now();
    assert!(matches!(
        expired.approve(&original, &approved),
        Err(Error::Expired)
    ));
    let invalid = plan(&dir, 2222, "fixture-user", KEY, "search text");
    assert!(matches!(
        Pending::request(original, invalid),
        Err(Error::InvalidApproval)
    ));
}

#[test]
fn ssh_askpass_ignores_inherited_sudo_environment() {
    let mut command = CommandBuilder::new("ssh");
    command.env("SUDO_ASKPASS", "/tmp/zzabba-elevate");
    command.env(super::sudo::MODE_ENV, "sudo");
    command.env("BW_SESSION", "inherited-session");
    super::remove_ssh_inherited(&mut command);
    assert_eq!(command.get_env("SUDO_ASKPASS"), None);
    assert_eq!(command.get_env(super::sudo::MODE_ENV), None);
    assert_eq!(command.get_env("BW_SESSION"), None);
}

#[test]
fn an_unacknowledged_response_is_not_cancelled() {
    let root = tempfile::tempdir().unwrap();
    let plan = plan(&root.path().join("trust"), 2222, "fixture-user", KEY, ITEM);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        for (attempted, expected) in [(false, Error::Cancelled), (true, Error::ResponseUnknown)] {
            let (ready, observed) = tokio::sync::oneshot::channel();
            let mut exchange =
                Exchange::start(Instant::now() + TTL, plan.clone(), |_, sent| async move {
                    sent.store(attempted, Ordering::Relaxed);
                    ready.send(()).unwrap();
                    std::future::pending().await
                });
            observed.await.unwrap();
            exchange.cancel();
            assert_eq!(exchange.result().await, Err(expected));
        }
    });
}

#[test]
fn cancelling_exchange_drops_inflight_lookup() {
    struct Guard(Arc<AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let root = tempfile::tempdir().unwrap();
    let plan = plan(&root.path().join("trust"), 2222, "fixture-user", KEY, ITEM);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Guard(Arc::clone(&dropped));
        let started = Arc::new(tokio::sync::Notify::new());
        let notify = Arc::clone(&started);
        let mut exchange = Exchange::start(Instant::now() + TTL, plan, move |_, _| async move {
            let _guard = guard;
            notify.notify_one();
            std::future::pending::<Result<(), Error>>().await
        });
        started.notified().await;
        exchange.cancel();
        assert_eq!(exchange.result().await, Err(Error::Cancelled));
        assert!(dropped.load(Ordering::Relaxed));
    });
}

#[test]
fn writable_install_and_missing_cli_are_distinct() {
    let temp = tempfile::tempdir().unwrap();
    let name = if cfg!(windows) { "bw.exe" } else { "bw" };
    let path = temp.path().join(name);
    assert!(matches!(
        Bitwarden::new(&path, None),
        Err(Error::VaultUnavailable)
    ));
    fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
    assert!(matches!(
        Bitwarden::new(&path, None),
        Err(Error::UntrustedExecutable)
    ));
}

#[test]
#[ignore = "로컬 OpenSSH, 제품 askpass 바이너리, ZZABBA_FIXTURE_PYTHON의 Paramiko가 필요하다"]
fn isolated_openssh_password_contract() {
    let python = std::env::var_os("ZZABBA_FIXTURE_PYTHON").expect("Paramiko Python executable");
    let helper = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(HELPER)
        .canonicalize()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let bw = dir.path().join(if cfg!(windows) { "bw.exe" } else { "bw" });
    compile_fixture(&bw);
    assert!(matches!(
        peer::installed_helper(),
        Err(Error::UntrustedExecutable)
    ));
    let helper = regular_path(&helper);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        for mode in ["success", "wrong-peer", "cancel", "wrong-target"] {
            scenario(mode, &python, &helper, &bw).await;
        }
    });
}

struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct PtyChild(Box<dyn Child + Send + Sync>);
impl Drop for PtyChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

async fn scenario(mode: &str, python: &OsStr, helper: &Path, bw: &Path) {
    let home = bw.parent().unwrap();
    let events = home.join("events");
    if events.exists() {
        fs::remove_file(&events).unwrap();
    }
    fs::write(home.join("mode"), mode).unwrap();
    let root = tempfile::tempdir().unwrap();
    let ready = root.path().join("allow-ssh");
    let mut server = start_server(python, &ready);
    let mut address = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let (port, key) = address
        .trim()
        .split_once(' ')
        .expect("fixture address and host key");
    let port = port.parse().unwrap();
    let directory = root.path().join("trust");
    let planned = plan(&directory, port, "fixture-user", key, ITEM);
    let pair = native_pty_system().openpty(PtySize::default()).unwrap();
    let observed = Mutex::new(None);
    let mut spawn = |command: CommandBuilder| {
        assert_eq!(
            command.get_env("SSH_ASKPASS_REQUIRE"),
            Some(OsStr::new("force"))
        );
        assert_eq!(command.get_env("BW_SESSION"), None);
        *observed.lock().unwrap() = command.get_env(ENDPOINT_ENV).map(OsStr::to_owned);
        pair.slave
            .spawn_command(command)
            .map_err(|_| Error::ChildUnavailable)
    };
    let fixture = Bitwarden::fixture(
        peer::TrustedFile::fixture(bw),
        Zeroizing::new("isolated-fixture-session".into()),
    );
    let current = document();
    let approve = Pending::request(current.clone(), planned.clone())
        .unwrap()
        .approve(&current, &planned)
        .unwrap();
    if mode == "wrong-target" {
        let other = plan(
            &directory,
            if port == 22 { 2222 } else { 22 },
            "other-user",
            KEY,
            ITEM,
        );
        assert!(matches!(
            approve.spawn(
                &current,
                &other,
                |_| panic!("wrong target reached child spawn"),
                fixture,
                peer::TrustedFile::fixture(helper)
            ),
            Err(Error::InvalidApproval)
        ));
        assert!(!events.exists());
        eprintln!("wrong target: status=0, get=0, response=0");
        return;
    }
    let (child, mut exchange) = approve
        .spawn(
            &current,
            &planned,
            &mut spawn,
            fixture,
            peer::TrustedFile::fixture(helper),
        )
        .unwrap();
    let mut child = PtyChild(child);
    let endpoint = observed.lock().unwrap().take().unwrap();
    drop(pair.slave);
    let (reader, snapshot) = read_pty(&*pair.master);
    #[cfg(windows)]
    let _pty_input = {
        // ConPTY가 출력 초기화에 쓰는 CPR이다. SSH에 암호나 셸 입력을 보내지 않는다.
        use std::io::Write;
        tokio::time::timeout(Duration::from_secs(3), async {
            while !snapshot.lock().unwrap().windows(4).any(|w| w == b"\x1b[6n") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("ConPTY cursor query");
        let mut writer = pair.master.take_writer().unwrap();
        writer.write_all(b"\x1b[1;1R").unwrap();
        writer
    };

    if mode == "wrong-peer" {
        // 제품 바이너리와 같은 OS 사용자를 사용해도 다른 부모는 0회다.
        let foreign = helper_command(helper, &endpoint).spawn().unwrap();
        let output = tokio::task::spawn_blocking(move || foreign.wait_with_output().unwrap())
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        eprintln!("unrelated peer: status=0, get=0, response=0");
        assert!(!events.exists());
    }
    if mode == "cancel" {
        exchange.cancel();
    }
    fs::write(&ready, b"").unwrap();
    let status = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{mode}: SSH child did not exit; bw={:?}; PTY={:?}",
            fs::read_to_string(&events),
            pty_snapshot(&snapshot)
        )
    });
    let result = tokio::time::timeout(Duration::from_secs(3), exchange.result())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{mode}: no askpass result; SSH={status}; bw={:?}; PTY={:?}",
                fs::read_to_string(&events),
                pty_snapshot(&snapshot)
            );
        });
    #[cfg(windows)]
    drop(_pty_input);
    drop(pair.master);
    let stdout = reader.join().unwrap();
    assert!(
        !stdout.contains("only-a-fixture-password"),
        "{mode}: private output reached PTY"
    );
    let expected = match mode {
        "success" | "wrong-peer" => (Ok(()), "status\nget\n"),
        "cancel" => (Err(Error::Cancelled), ""),
        _ => unreachable!(),
    };
    assert_eq!(result, expected.0, "{mode}: {stdout:?}");
    assert_eq!(
        fs::read_to_string(&events).unwrap_or_default(),
        expected.1,
        "{mode}: CLI calls"
    );
    assert_eq!(status.success(), expected.0.is_ok(), "{mode}: {stdout:?}");
    assert_eq!(
        stdout.contains("FIXTURE-AUTH-OK"),
        expected.0.is_ok(),
        "{mode}: {stdout:?}"
    );
    let replay = helper_command(helper, &endpoint).output().unwrap();
    assert!(!replay.status.success());
    assert!(replay.stdout.is_empty());
    assert_eq!(fs::read_to_string(&events).unwrap_or_default(), expected.1);
    eprintln!("replay after {mode}: additional status=0, get=0, response=0");
    eprintln!(
        "{mode}: status={}, get={}, response={}",
        expected.1.lines().filter(|line| *line == "status").count(),
        expected.1.lines().filter(|line| *line == "get").count(),
        usize::from(result.is_ok())
    );
}

fn pty_snapshot(snapshot: &Mutex<Vec<u8>>) -> String {
    String::from_utf8_lossy(&snapshot.lock().unwrap()).into_owned()
}

fn start_server(python: &OsStr, ready: &Path) -> Server {
    let script = Path::new(file!())
        .parent()
        .unwrap()
        .join("tests/ssh_fixture.py");
    Server(
        Command::new(python)
            .arg(script)
            .arg(ready)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}

fn compile_fixture(bw: &Path) {
    let source = Path::new(file!())
        .parent()
        .unwrap()
        .join("tests/bw_fixture.rs");
    assert!(
        Command::new("rustc")
            .arg(source)
            .arg("-o")
            .arg(bw)
            .status()
            .unwrap()
            .success()
    );
}

fn read_pty(master: &dyn MasterPty) -> (std::thread::JoinHandle<String>, Arc<Mutex<Vec<u8>>>) {
    let mut reader = master.try_clone_reader().unwrap();
    let snapshot = Arc::new(Mutex::new(Vec::new()));
    let captured = snapshot.clone();
    let task = std::thread::spawn(move || {
        let mut buf = [0; 4096];
        while let Ok(n @ 1..) = reader.read(&mut buf) {
            captured.lock().unwrap().extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&captured.lock().unwrap()).into_owned()
    });
    (task, snapshot)
}

fn helper_command(path: &Path, endpoint: &OsStr) -> Command {
    let mut command = Command::new(path);
    command
        .env("SSH_ASKPASS", path)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env_remove("BW_SESSION")
        .env_remove("SSH_ASKPASS_PROMPT")
        .env(BROKER_ENV, std::process::id().to_string())
        .env(ENDPOINT_ENV, endpoint)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn regular_path(path: &Path) -> PathBuf {
    if cfg!(windows) {
        let path = path.to_str().unwrap();
        return PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(path));
    }
    path.to_owned()
}

// 금고 오류 매핑은 실제 대화형 세션 없이 fixture 모드로 고정한다.
#[tokio::test]
async fn vault_reports_locked_and_missing_items_from_bw_output() {
    let dir = tempfile::tempdir().unwrap();
    let bw = dir.path().join(if cfg!(windows) { "bw.exe" } else { "bw" });
    compile_fixture(&bw);
    let mode = dir.path().join("mode");
    for (value, expected) in [
        ("locked", Error::VaultLocked),
        ("missing", Error::ItemMissing),
    ] {
        fs::write(&mode, value).unwrap();
        let vault = Bitwarden::fixture(
            peer::TrustedFile::fixture(&bw),
            Zeroizing::new("isolated-fixture-session".into()),
        );
        assert_eq!(vault.password(ITEM).await, Err(expected), "{value}");
    }
}
