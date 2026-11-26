use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use portable_pty::{Child, MasterPty, PtyPair, PtySize, native_pty_system};
use zeroize::Zeroizing;

use super::*;
use crate::local_auth::{Bitwarden, Error, Exchange, TrustedFile};

const ITEM: &str = "21eb36c3-234d-439d-9e1f-1d3cc5ba8125";

struct Fixture {
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().unwrap(),
        }
    }

    fn sudo(&self) -> PathBuf {
        let directory = self.directory.path().join("sudo");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(exe("sudo"));
        compile(&fixture_source(), &path);
        path
    }

    fn mode(&self, mode: &str) {
        let directory = self.directory.path().join("sudo");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("mode"), mode).unwrap();
    }

    fn events(&self, name: &str) -> String {
        std::fs::read_to_string(self.directory.path().join(name).join("events")).unwrap_or_default()
    }

    fn bw(&self) -> Bitwarden {
        let directory = self.directory.path().join("bw");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(exe("bw"));
        compile(&bw_source(), &path);
        Bitwarden::fixture(
            TrustedFile::fixture(&path),
            Zeroizing::new("isolated-fixture-session".into()),
        )
    }
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.into() }
}

fn fixture_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/auth/sudo/tests/sudo_fixture.rs")
}

fn bw_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/auth/tests/bw_fixture.rs")
}

fn compile(source: &Path, output: &Path) {
    let status = Command::new("rustc")
        .arg("--edition=2021")
        .arg(source)
        .arg("-o")
        .arg(output)
        .status()
        .unwrap();
    assert!(status.success(), "{}", source.display());
}

fn helper() -> PathBuf {
    let path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(super::super::HELPER);
    assert!(path.is_file(), "zxt-askpass를 먼저 빌드해야 합니다: {}", path.display());
    path
}

fn pty() -> PtyPair {
    native_pty_system().openpty(PtySize::default()).unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

struct Running {
    child: PtyChild,
    exchange: Exchange,
    reader: std::thread::JoinHandle<String>,
    master: Box<dyn MasterPty + Send>,
    #[cfg(windows)]
    _input: Box<dyn Write + Send>,
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

async fn start(fixture: &Fixture, pair: PtyPair, vault: Bitwarden) -> Running {
    let (child, exchange) = spawn(
        Path::new("/usr/bin/id"),
        &["-u".to_owned()],
        TrustedFile::fixture(&helper()),
        TrustedFile::fixture(&fixture.sudo()),
        vault,
        ITEM,
        |command| pair.slave.spawn_command(command).map_err(|_| Error::ChildUnavailable),
    ).unwrap();
    drop(pair.slave);
    let (reader, snapshot) = read_pty(&*pair.master);
    #[cfg(windows)]
    let _input = answer_cpr(&*pair.master, &snapshot).await;
    Running {
        child: PtyChild(child),
        exchange,
        reader,
        master: pair.master,
        #[cfg(windows)]
        _input,
    }
}

fn read_pty(master: &dyn MasterPty) -> (std::thread::JoinHandle<String>, Arc<Mutex<Vec<u8>>>) {
    let mut reader = master.try_clone_reader().unwrap();
    let snapshot = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&snapshot);
    let task = std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(count @ 1..) = reader.read(&mut buffer) {
            captured.lock().unwrap().extend_from_slice(&buffer[..count]);
        }
        String::from_utf8_lossy(&captured.lock().unwrap()).into_owned()
    });
    (task, snapshot)
}

#[cfg(windows)]
async fn answer_cpr(master: &dyn MasterPty, snapshot: &Arc<Mutex<Vec<u8>>>) -> Box<dyn Write + Send> {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !snapshot
            .lock()
            .unwrap()
            .windows(4)
            .any(|window| window == b"\x1b[6n")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("ConPTY 커서 위치 질의");
    let mut writer = master.take_writer().unwrap();
    writer.write_all(b"\x1b[1;1R").unwrap();
    writer
}

async fn wait_exit(child: &mut dyn Child) -> portable_pty::ExitStatus {
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn elevation_answers_one_askpass_and_keeps_the_password_out_of_the_pty() {
    let fixture = Fixture::new();
    fixture.mode("success");
    let vault = fixture.bw();
    runtime().block_on(async {
        let mut run = start(&fixture, pty(), vault).await;
        let status = wait_exit(&mut *run.child.0).await;
        let result = tokio::time::timeout(Duration::from_secs(5), run.exchange.result()).await.unwrap();
        let output = finish(run).await;
        assert!(status.success());
        assert_eq!(result, Ok(()));
        assert!(output.contains("SUDO-RAN true"), "{output}");
        assert!(!output.contains("only-a-fixture-password"), "{output}");
        assert_eq!(fixture.events("bw"), "status\nget\n");
        let events = fixture.events("sudo");
        assert!(events.contains("ask1=true"), "{events}");
        assert!(events.contains("askpass=true"), "{events}");
        assert!(events.contains("ssh_askpass=false"), "{events}");
        assert!(events.contains("run -A -- /usr/bin/id -u"), "{events}");
    });
}

#[test]
fn elevation_uses_no_vault_when_sudo_auth_is_cached() {
    let fixture = Fixture::new();
    fixture.mode("cached");
    let vault = fixture.bw();
    runtime().block_on(async {
        let mut run = start(&fixture, pty(), vault).await;
        let status = wait_exit(&mut *run.child.0).await;
        let result = tokio::time::timeout(Duration::from_secs(5), run.exchange.result()).await.unwrap();
        let output = finish(run).await;
        assert!(status.success());
        assert_eq!(result, Ok(()));
        assert!(output.contains("SUDO-CACHED"), "{output}");
        assert_eq!(fixture.events("bw"), "");
        assert!(!fixture.events("sudo").contains("ask1"), "{}", fixture.events("sudo"));
    });
}

#[test]
fn elevation_consumes_one_response_and_rejects_replay() {
    let fixture = Fixture::new();
    fixture.mode("replay");
    let vault = fixture.bw();
    runtime().block_on(async {
        let mut run = start(&fixture, pty(), vault).await;
        let status = wait_exit(&mut *run.child.0).await;
        let result = tokio::time::timeout(Duration::from_secs(10), run.exchange.result()).await.unwrap();
        let output = finish(run).await;
        assert!(status.success());
        assert_eq!(result, Ok(()));
        assert!(output.contains("REPLAY true false"), "{output}");
        assert_eq!(fixture.events("bw"), "status\nget\n");
        let events = fixture.events("sudo");
        assert_eq!(events.lines().filter(|line| *line == "ask1=true").count(), 1, "{events}");
        assert!(events.contains("ask2=false"), "{events}");
    });
}

#[test]
fn elevation_cancel_before_askpass_performs_no_vault_lookup() {
    let fixture = Fixture::new();
    fixture.mode("slow");
    let vault = fixture.bw();
    runtime().block_on(async {
        let mut run = start(&fixture, pty(), vault).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !fixture.events("sudo").contains("waiting") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        run.exchange.cancel();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), run.exchange.result()).await.unwrap(),
            Err(Error::Cancelled)
        );
        let _ = wait_exit(&mut *run.child.0).await;
        let output = finish(run).await;
        assert_eq!(fixture.events("bw"), "");
        assert!(!output.contains("only-a-fixture-password"), "{output}");
    });
}

#[test]
fn elevation_rejects_a_helper_outside_the_sudo_child_lineage() {
    let fixture = Fixture::new();
    fixture.mode("hold");
    let vault = fixture.bw();
    runtime().block_on(async {
        let mut run = start(&fixture, pty(), vault).await;
        let endpoint = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(value) = fixture
                    .events("sudo")
                    .lines()
                    .find_map(|line| line.strip_prefix("endpoint="))
                {
                    return value.to_owned();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut foreign = Command::new(helper())
            .env("SUDO_ASKPASS", helper())
            .env("ZXT_ASKPASS_MODE", "sudo")
            .env("ZXT_ASKPASS_ENDPOINT", endpoint)
            .env("ZXT_ASKPASS_BROKER_PID", std::process::id().to_string())
            .env_remove("SSH_ASKPASS")
            .env_remove("SSH_ASKPASS_REQUIRE")
            .env_remove("BW_SESSION")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while foreign.try_wait().unwrap().is_none() {
            if tokio::time::Instant::now() >= deadline {
                let _ = foreign.kill();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let output = foreign.wait_with_output().unwrap();
        assert!(!output.status.success(), "계보 밖 helper가 승인을 받았습니다: {output:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(fixture.events("bw"), "");
        run.exchange.cancel();
        let _ = wait_exit(&mut *run.child.0).await;
        let _ = finish(run).await;
    });
}

async fn finish(run: Running) -> String {
    drop(run.master);
    run.reader.join().unwrap()
}

#[test]
fn elevation_helper_rejects_wrong_mode_environment() {
    let helper = helper();
    let other = std::env::current_exe().unwrap();
    for environment in [
        vec![("SUDO_ASKPASS", other.as_os_str()), ("ZXT_ASKPASS_MODE", "sudo".as_ref())],
        vec![
            ("SUDO_ASKPASS", helper.as_os_str()),
            ("ZXT_ASKPASS_MODE", "sudo".as_ref()),
            ("SSH_ASKPASS_REQUIRE", "force".as_ref()),
        ],
        vec![
            ("SSH_ASKPASS", helper.as_os_str()),
            ("SSH_ASKPASS_REQUIRE", "force".as_ref()),
            ("SUDO_ASKPASS", helper.as_os_str()),
        ],
    ] {
        let mut command = Command::new(&helper);
        command
            .env("ZXT_ASKPASS_ENDPOINT", "/nonexistent")
            .env("ZXT_ASKPASS_BROKER_PID", "1")
            .env_remove("BW_SESSION")
            .env_remove("SSH_ASKPASS_PROMPT");
        for (key, value) in environment {
            command.env(key, value);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn elevation_rejects_a_writable_helper_without_opening_the_vault() {
    let fixture = Fixture::new();
    let directory = fixture.directory.path().join("writable");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(exe(super::super::HELPER));
    std::fs::copy(helper(), &path).unwrap();
    assert!(matches!(
        TrustedFile::check(&path, super::super::HELPER),
        Err(Error::UntrustedExecutable)
    ));
}

#[test]
fn elevation_mismatched_approval_never_opens_the_vault() {
    use crate::elevation::{Document, Pending, Request};
    let fixture = Fixture::new();
    let vault = fixture.bw();
    let pair = pty();
    let document = Document {
        owner: "main".into(),
        document: 1,
        request: "a".repeat(32),
    };
    let fixed = Request::new(
        "demo".into(),
        std::env::current_exe().unwrap(),
        vec!["-u".into()],
        ITEM.into(),
    );
    let approval = Pending::request(document.clone(), fixed.clone())
        .unwrap()
        .approve(&document, &fixed)
        .unwrap();
    let other = Request {
        args: vec!["-u".into(), "extra".into()],
        ..fixed.clone()
    };
    let result = approval.spawn_local(
        &document,
        &other,
        &*pair.slave,
        TrustedFile::fixture(&helper()),
        TrustedFile::fixture(&fixture.sudo()),
        vault,
    );
    assert!(matches!(result, Err(Error::InvalidApproval)));
    assert_eq!(fixture.events("bw"), "");
}

#[test]
fn elevation_launch_starts_without_an_outer_runtime() {
    use crate::elevation::{Document, Launch, Pending, Request};
    let fixture = Fixture::new();
    fixture.mode("cached");
    let vault = fixture.bw();
    let pair = pty();
    let document = Document {
        owner: "main".into(),
        document: 1,
        request: "b".repeat(32),
    };
    let request = Request::new(
        "demo".into(),
        std::env::current_exe().unwrap(),
        vec!["-u".into()],
        ITEM.into(),
    );
    let approval = Pending::request(document.clone(), request.clone())
        .unwrap()
        .approve(&document, &request)
        .unwrap();
    let launch = Launch::new(
        approval,
        document,
        request,
        TrustedFile::fixture(&helper()),
        TrustedFile::fixture(&fixture.sudo()),
        vault,
    );
    // PTY 감독자 스레드와 같이 tokio 컨텍스트 없이 호출한다.
    let (child, mut exchange) = launch.spawn(&*pair.slave).unwrap();
    drop(pair.slave);
    let (reader, snapshot) = read_pty(&*pair.master);
    runtime().block_on(async {
        #[cfg(windows)]
        let _input = answer_cpr(&*pair.master, &snapshot).await;
        let mut child = PtyChild(child);
        let status = wait_exit(&mut *child.0).await;
        let result = tokio::time::timeout(Duration::from_secs(5), exchange.result()).await.unwrap();
        drop(pair.master);
        let output = reader.join().unwrap();
        assert!(status.success());
        assert_eq!(result, Ok(()));
        assert!(output.contains("SUDO-CACHED"), "{output}");
        assert_eq!(fixture.events("bw"), "");
    });
}
