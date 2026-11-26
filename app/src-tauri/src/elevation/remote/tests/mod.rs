use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::Ordering,
};

use zeroize::Zeroizing;

use super::*;
use crate::local_auth::TrustedFile;

const ID: &str = "0123456789abcdef0123456789abcdef";
const NONCE: &str = "fedcba9876543210fedcba9876543210";
const ITEM: &str = "21eb36c3-234d-439d-9e1f-1d3cc5ba8125";

struct Fixture {
    directory: tempfile::TempDir,
    bridge: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let bridge = directory.path().join(exe("bridge"));
        compile(&source("bridge_fixture.rs"), &bridge);
        Self { directory, bridge }
    }

    fn vault(&self) -> (PathBuf, Bitwarden) {
        let directory = self.directory.path().join("bw");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(exe("bw"));
        compile(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/auth/tests/bw_fixture.rs"),
            &path,
        );
        let vault = Bitwarden::fixture(
            TrustedFile::fixture(&path),
            Zeroizing::new("isolated-fixture-session".into()),
        );
        (directory, vault)
    }

    fn bridge(&self, mode: &str, vault: Bitwarden) -> Bridge {
        let mut command = Command::new(&self.bridge);
        command.args([
            self.directory.path().to_str().unwrap(),
            ID,
            NONCE,
            mode,
        ]);
        Bridge {
            command,
            id: ID.into(),
            nonce: NONCE.into(),
            item: ITEM.into(),
            vault,
        }
    }

    fn bridge_events(&self) -> String {
        std::fs::read_to_string(self.directory.path().join("bridge-events")).unwrap_or_default()
    }
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.into() }
}

fn source(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/elevation/remote/tests")
        .join(name)
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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn state() -> Arc<State> {
    Arc::new(State {
        run_done: CancellationToken::new(),
        cancel: CancellationToken::new(),
        unconfirmed: AtomicBool::new(false),
        abort: AtomicBool::new(false),
        sent: AtomicBool::new(false),
        _plan: None,
    })
}

#[test]
fn elevation_remote_verify_requires_root_helper_and_same_account() {
    let good = "zxt-elevate 1\nuser=demo\nuid=1001\nowner=0\nmode=755\npath=/usr/local/libexec/zxt-elevate\n";
    assert!(parse_verify(good, "demo").is_some());
    for (text, account) in [
        (good.replace("user=demo", "user=other"), "demo"),
        (good.replace("owner=0", "owner=1001"), "demo"),
        (good.replace("mode=755", "mode=775"), "demo"),
        (good.replace(HELPER, "/tmp/zxt-elevate"), "demo"),
        (good.replace("zxt-elevate 1", "zxt-elevate 2"), "demo"),
        (String::new(), "demo"),
    ] {
        assert!(parse_verify(&text, account).is_none(), "{text}");
    }
}

#[test]
fn elevation_remote_run_frame_keeps_approval_id_nonce_and_argument_boundaries() {
    let frame = run_frame(ID, NONCE, "/usr/bin/printf", &["a b".into(), "c".into()]).unwrap();
    let fields: Vec<&str> = frame.trim_end().split(' ').collect();
    assert_eq!(fields.len(), 8);
    assert_eq!(&fields[..4], ["ZXT-ELEVATE", "1", ID, NONCE]);
    assert_eq!(fields[5], "2");
    assert!(fields[6..].iter().all(|field| field.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(&byte))));
    assert!(!frame.contains("a b"));
    assert!(run_frame(ID, NONCE, "relative/id", &[]).is_err());
    assert!(run_frame("short", NONCE, "/usr/bin/id", &[]).is_err());
    assert!(run_frame(ID, NONCE, "/usr/bin/id", &(0..65).map(|_| "x".into()).collect::<Vec<_>>()).is_err());
}

#[test]
fn elevation_remote_bridge_request_rejects_truncation_sequence_and_other_approval() {
    assert!(parse_request(&format!("REQUEST 2 {ID} {NONCE}\n"), ID, NONCE).is_ok());
    for line in [
        format!("REQUEST 1 {ID} {NONCE}\n"),
        format!("REQUEST 3 {ID} {NONCE}\n"),
        format!("REQUEST 2 {ID}\n"),
        format!("REQUEST 2 {ID} {NONCE} extra\n"),
        format!("REQUEST 2 {ID} {}\n", "0".repeat(32)),
        format!("REQUEST 2 {} {NONCE}\n", "0".repeat(32)),
        "REQUEST 2\n".into(),
        String::new(),
    ] {
        assert!(parse_request(&line, ID, NONCE).is_err(), "{line:?}");
    }
}

#[test]
fn elevation_remote_bridge_rejects_bad_request_without_vault_lookup() {
    for mode in ["invalid", "truncated", "missing"] {
        let fixture = Fixture::new();
        let (vault_directory, vault) = fixture.vault();
        let result = runtime().block_on(fixture.bridge(mode, vault).run(state()));
        assert!(matches!(result, Err(Error::ChannelUnavailable)), "{mode}: {result:?}");
        assert_eq!(
            std::fs::read_to_string(vault_directory.join("events")).unwrap_or_default(),
            "",
            "{mode}: 금고 조회가 있었습니다"
        );
        assert_eq!(fixture.bridge_events(), "", "{mode}");
    }
}

#[test]
fn elevation_remote_bridge_sends_one_password_after_request() {
    let fixture = Fixture::new();
    let (vault_directory, vault) = fixture.vault();
    let result = runtime().block_on(fixture.bridge("ok", vault).run(state()));
    assert_eq!(result, Ok(()));
    assert_eq!(
        std::fs::read_to_string(vault_directory.join("events")).unwrap_or_default(),
        "status\nget\n"
    );
    assert_eq!(fixture.bridge_events(), "1 true\n");
}

#[test]
fn elevation_remote_channel_loss_marks_result_unknown() {
    for mode in ["missing", "fail"] {
        let fixture = Fixture::new();
        let (_, vault) = fixture.vault();
        let state = state();
        let mut notified = None;
        watch(fixture.bridge(mode, vault), Arc::clone(&state), |message| notified = Some(message));
        assert!(state.unconfirmed(), "{mode}");
        assert_eq!(state.abort(), mode == "missing", "{mode}");
        assert!(notified.is_some_and(|message| message.contains("결과 미확인")), "{mode}: {notified:?}");
    }
}

#[test]
fn elevation_remote_vault_failure_aborts_before_any_password() {
    let fixture = Fixture::new();
    let (vault_directory, vault) = fixture.vault();
    std::fs::write(vault_directory.join("mode"), "locked").unwrap();
    let state = state();
    let mut notified = None;
    watch(fixture.bridge("ok", vault), Arc::clone(&state), |message| notified = Some(message));
    assert!(state.unconfirmed());
    assert!(state.abort());
    assert!(!state.sent.load(Ordering::Relaxed));
    assert!(notified.is_some_and(|message| message.contains("금고")));
}

#[test]
fn elevation_remote_channel_loss_after_run_exit_is_known() {
    let fixture = Fixture::new();
    let (_, vault) = fixture.vault();
    let state = state();
    state.run_done.cancel();
    watch(fixture.bridge("missing", vault), Arc::clone(&state), |_| panic!("run 종료 뒤에는 알림이 없어야 합니다"));
    assert!(!state.unconfirmed());
    assert!(!state.sent.load(Ordering::Relaxed));
}

#[test]
#[ignore = "sudo가 설치된 시험 컨테이너와 .omc 원격 JSON이 필요하다"]
fn elevation_remote_real_container_approves_once_and_runs_sudo() {
    use crate::ssh::{ApprovedHost, Authentication, ConnectionPlan, Endpoint, Transport, TrustStore};
    use std::{fs, path::PathBuf};
    use ssh_key::HashAlg;

    let path = std::env::var("ZXT_SUDO_REMOTE").expect("ZXT_SUDO_REMOTE JSON 경로");
    let data: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let host = data["host"].as_str().unwrap();
    let port: u16 = data["port"].as_u64().unwrap() as u16;
    let account = data["account"].as_str().unwrap();
    let private_key = PathBuf::from(data["private_key"].as_str().unwrap());
    let host_public_key = data["host_public_key"].as_str().unwrap();
    let password = data["password"].as_str().unwrap();

    let fixture = Fixture::new();
    let (vault_directory, vault) = fixture.vault();
    fs::write(vault_directory.join("password"), password).unwrap();

    let trust_directory = fixture.directory.path().join("trust");
    let store = TrustStore::create(&trust_directory).unwrap();
    let endpoint = Endpoint::new(host, port).unwrap();
    let fingerprint = ssh_key::PublicKey::from_openssh(host_public_key)
        .unwrap()
        .fingerprint(HashAlg::Sha256)
        .to_string();
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, &fingerprint).unwrap();
    store.register(&approved, host_public_key).unwrap();
    let plan = Arc::new(
        ConnectionPlan::prepare(
            &store,
            &approved,
            account,
            Transport::Direct,
            Authentication::KeyFile { private_key: &private_key },
        )
        .unwrap(),
    );
    verify(&plan, account).unwrap();

    let document = crate::elevation::Document {
        owner: "main".into(),
        document: 1,
        request: "c".repeat(32),
    };
    let request = crate::elevation::Request::new_remote(
        format!("{account}@{host}:{port}"),
        account.into(),
        PathBuf::from("/usr/bin/id"),
        vec!["-u".into()],
        ITEM.into(),
    );
    let pending = crate::elevation::Pending::request(document.clone(), request.clone()).unwrap();
    let _approval = pending.approve(&document, &request).unwrap();
    let launch = Launch::build(Arc::clone(&plan), &document, &request, vault).unwrap();
    let id = launch.bridge.id().to_owned();
    let nonce = launch.bridge.nonce().to_owned();
    let config = launch.state.plan_config().expect("승격 상태가 계획을 보관해야 합니다");
    assert!(config.is_file(), "계획 임시 SSH 설정이 승격 세션보다 먼저 삭제되었습니다");
    let frame = launch.frame.clone();
    let state = Arc::clone(&launch.state);
    // sudo 캐시를 지워 이 실행이 반드시 askpass를 쓰게 한다.
    let mut clear = crate::ssh::terminal::remote_channel(
        &plan,
        &["/usr/bin/sudo".into(), "-k".into()],
    )
    .unwrap()
    .piped;
    clear.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut run = crate::ssh::terminal::remote_channel(
        &plan,
        &[HELPER.to_owned(), "serve".into(), id, nonce],
    )
    .unwrap()
    .piped;
    run.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // IPC가 반환하며 원본 계획을 버려도 세션 상태가 설정 파일을 유지해야 한다.
    drop(plan);
    assert!(config.is_file(), "원본 계획이 사라진 뒤에도 설정이 남아야 합니다");
    assert!(clear.status().unwrap().success());

    runtime().block_on(async {
        let bridge = tokio::spawn(launch.bridge.run(Arc::clone(&state)));
        let mut child = TokioCommand::from(run)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(&frame).await.unwrap();
        stdin.shutdown().await.unwrap();
        drop(stdin);
        let mut output = Vec::new();
        child.stdout.take().unwrap().read_to_end(&mut output).await.unwrap();
        let mut error = Vec::new();
        child.stderr.take().unwrap().read_to_end(&mut error).await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(60), child.wait()).await.unwrap().unwrap();
        let bridge = tokio::time::timeout(Duration::from_secs(60), bridge).await.unwrap().unwrap();
        let text = String::from_utf8_lossy(&output);
        eprintln!("remote stdout={text:?} stderr={:?}", String::from_utf8_lossy(&error));
        assert!(status.success(), "{text}");
        assert_eq!(bridge, Ok(()));
        assert!(state.sent.load(Ordering::Relaxed));
        assert!(text.lines().any(|line| line.trim() == "0"), "{text}");
        assert!(!text.contains(password));
        assert_eq!(
            std::fs::read_to_string(vault_directory.join("events")).unwrap_or_default(),
            "status\nget\n"
        );
    });
}

#[test]
#[ignore = "sudo가 설치된 시험 컨테이너와 .omc 원격 JSON이 필요하다"]
fn elevation_remote_askpass_times_out_when_bridge_never_answers() {
    use crate::ssh::{ApprovedHost, Authentication, ConnectionPlan, Endpoint, Transport, TrustStore};
    use std::{fs, io::Write, path::PathBuf};
    use ssh_key::HashAlg;

    let path = std::env::var("ZXT_SUDO_REMOTE").expect("ZXT_SUDO_REMOTE JSON 경로");
    let data: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let host = data["host"].as_str().unwrap();
    let port: u16 = data["port"].as_u64().unwrap() as u16;
    let account = data["account"].as_str().unwrap();
    let private_key = PathBuf::from(data["private_key"].as_str().unwrap());
    let host_public_key = data["host_public_key"].as_str().unwrap();
    let trust_directory = tempfile::tempdir().unwrap();
    let store = TrustStore::create(&trust_directory.path().join("trust")).unwrap();
    let endpoint = Endpoint::new(host, port).unwrap();
    let fingerprint = ssh_key::PublicKey::from_openssh(host_public_key)
        .unwrap()
        .fingerprint(HashAlg::Sha256)
        .to_string();
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, &fingerprint).unwrap();
    store.register(&approved, host_public_key).unwrap();
    let plan = Arc::new(
        ConnectionPlan::prepare(
            &store,
            &approved,
            account,
            Transport::Direct,
            Authentication::KeyFile { private_key: &private_key },
        )
        .unwrap(),
    );
    verify(&plan, account).unwrap();
    let id = "d".repeat(32);
    let nonce = "e".repeat(32);
    let frame = run_frame(&id, &nonce, "/usr/bin/id", &["-u".into()]).unwrap();
    let mut bridge = crate::ssh::terminal::remote_channel(
        &plan,
        &[HELPER.to_owned(), "bridge".into(), id.clone(), nonce.clone()],
    )
    .unwrap()
    .piped;
    bridge.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut run = crate::ssh::terminal::remote_channel(
        &plan,
        &[HELPER.to_owned(), "serve".into(), id, nonce],
    )
    .unwrap()
    .piped;
    run.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut bridge = bridge.spawn().unwrap();
    let start = std::time::Instant::now();
    let mut child = run.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(frame.as_bytes()).unwrap();
    drop(stdin);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(start.elapsed() < Duration::from_secs(40), "askpass가 기한 없이 매달렸습니다");
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = bridge.wait();
    assert!(!status.success(), "bridge가 응답하지 않았는데 run이 성공했습니다");
    assert!(start.elapsed() < Duration::from_secs(40));
}
