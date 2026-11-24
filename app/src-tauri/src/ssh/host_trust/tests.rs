use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}};

use ssh_key::HashAlg;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::ssh::{TRUST_DIRECTORY, terminal::{ForwardLaunch, prepare_saved_with}};

const ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOIlkaatyE67xu4EHyRVTE4sDP2jV6I47yQt41UrC6LE";
const OTHER: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAp6FbiyNlbjvovJS0F4f1sLOMTggt3UUmcJ2HBF164Z";
const ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBNiaVoCbXHhP2UdGTfHD5w25SPlZfyv8Z97SkcZYepjM2ZFeZXqSuFv7MpMXy+LJJ+dPfyiyCrGIBLXCVXZlhnw=";
const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDotXB4+yx/RHy7KWe/zRRXTfa/GNy1YIyFZxFBFmbwOxWfIr/1JR7BWoqEkNsnyI/O7KmXbg5c56I4El6qwXnoGs9NRKmCEWd0QWBkkphEBMhDE07+VLxi7p4HWg4JAKv8BRQp9OvPebCEtyKbjN+T9IMgLaLI4jm+oBXbYGuyh+KO5cKdrVhsoEobw73uNatWIOmjtnRaRMlMlO5InZPxdVV3SY0HaSBKOMn7nQUj4PVXdnEx6EMJE+8Yy+ba1GpXQJjc3FxtJRqKuQNVOcTbnOxagfo9uFbZc3yiZ8FvxiT81GuguOyD9QJWp5kCwZNm5GrlqbMRzMG340U1wXAh";

fn sha(key: &str) -> String {
    plain_public_key(key).unwrap().fingerprint(HashAlg::Sha256).to_string()
}

fn target(fingerprint: Option<&str>) -> (tempfile::TempDir, Ssh) {
    let directory = tempfile::tempdir().unwrap();
    TrustStore::create(&directory.path().join(TRUST_DIRECTORY)).unwrap();
    let mut ssh = serde_json::json!({
        "host": "dev.example.invalid", "port": 2222, "account": "deploy",
        "transport": {"kind": "direct"}, "auth": {"kind": "manualPassword"},
    });
    if let Some(fingerprint) = fingerprint {
        ssh["hostKeyFingerprint"] = fingerprint.into();
    }
    (directory, serde_json::from_value(ssh).unwrap())
}

fn over_ssm(ssh: &mut Ssh) {
    ssh.transport = serde_json::from_value(serde_json::json!({
        "kind": "ssm", "profile": "dev-sso", "region": "ap-northeast-2", "instanceId": "i-0123456789abcdef0",
    })).unwrap();
}

fn store(directory: &tempfile::TempDir) -> TrustStore {
    TrustStore::open(&directory.path().join(TRUST_DIRECTORY)).unwrap()
}

fn endpoint() -> Endpoint {
    Endpoint::new("dev.example.invalid", 2222).unwrap()
}

fn pin(directory: &tempfile::TempDir, key: &str) {
    let endpoint = endpoint();
    let fingerprint = sha(key);
    store(directory).register(&ApprovedHost::confirm(&endpoint, &endpoint, &fingerprint).unwrap(), key).unwrap();
}

fn pinned_key(directory: &tempfile::TempDir) -> Option<String> {
    store(directory).pinned(&endpoint()).unwrap().map(|key| key.fingerprint(HashAlg::Sha256).to_string())
}

/// 띄운 확인 창의 문구를 기록하고 정해진 답을 돌려준다.
fn answering(answer: bool) -> (Confirm, Arc<Mutex<Vec<String>>>) {
    let shown = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&shown);
    (Arc::new(move |message| { record.lock().unwrap().push(message); Ok(answer) }), shown)
}

fn refuse_prompt() -> Confirm {
    Arc::new(|message| panic!("확인 창을 열었습니다: {message}"))
}

fn serve(key: &'static str) -> impl Fn(&str, u16) -> Result<String, String> {
    move |host, port| Ok(format!("# {host}:{port} SSH-2.0-OpenSSH_9.6\n[{host}]:{port} {key}\n"))
}

fn no_scan(_: &str, _: u16) -> Result<String, String> {
    panic!("등록된 핀이 있는데 서버 키를 받았습니다")
}

#[test]
fn scan_output_picks_ed25519_then_ecdsa_then_rsa_and_ignores_comments_and_bad_lines() {
    let pick = |output: &str| choose(output).map(|key| key.fingerprint(HashAlg::Sha256).to_string());
    let lines = [
        "# dev.example.invalid:22 SSH-2.0-OpenSSH_9.6".to_owned(),
        "[dev.example.invalid]:2222 ssh-ed25519 AAAA-not-base64".to_owned(),
        "garbage".to_owned(),
        format!("[dev.example.invalid]:2222 {RSA}"),
        format!("[dev.example.invalid]:2222 {ECDSA}"),
        format!("[dev.example.invalid]:2222 {ED25519}"),
    ];
    assert_eq!(pick(&lines.join("\n")), Some(sha(ED25519)));
    assert_eq!(pick(&lines[..5].join("\n")), Some(sha(ECDSA)));
    assert_eq!(pick(&lines[..4].join("\r\n")), Some(sha(RSA)));
    assert_eq!(pick(&lines[..3].join("\n")), None);
    // 주석 줄에 섞인 키는 받지 않는다.
    assert_eq!(pick(&format!("# {ED25519}")), None);
}

#[test]
fn existing_pin_skips_scan_and_prompt() {
    let (directory, ssh) = target(None);
    pin(&directory, ED25519);
    let refuse = refuse_prompt();
    assert_eq!(trusted(directory.path(), &ssh, Via::Direct, &no_scan, Some(&refuse)), Ok(sha(ED25519)));
    assert_eq!(required(directory.path(), &ssh), Ok(sha(ED25519)));
}

#[test]
fn missing_pin_prompts_with_host_port_type_and_sha256_then_registers() {
    let (directory, ssh) = target(None);
    let (confirm, shown) = answering(true);
    assert_eq!(trusted(directory.path(), &ssh, Via::Direct, &serve(ED25519), Some(&confirm)), Ok(sha(ED25519)));
    let shown = shown.lock().unwrap().clone();
    assert_eq!(shown, [format!("서버: dev.example.invalid\n포트: 2222\n키 종류: ssh-ed25519\nSHA256 지문: {}", sha(ED25519))]);
    assert_eq!(pinned_key(&directory), Some(sha(ED25519)));
    // 다시 연결할 때는 창을 띄우지 않고 서버 키도 다시 받지 않는다.
    let refuse = refuse_prompt();
    assert_eq!(trusted(directory.path(), &ssh, Via::Direct, &no_scan, Some(&refuse)), Ok(sha(ED25519)));
}

#[test]
fn declined_prompt_writes_no_pin() {
    let (directory, ssh) = target(None);
    let (confirm, shown) = answering(false);
    assert_eq!(trusted(directory.path(), &ssh, Via::Direct, &serve(ED25519), Some(&confirm)), Err(DECLINED.into()));
    assert_eq!(shown.lock().unwrap().len(), 1);
    assert_eq!(pinned_key(&directory), None);
}

#[test]
fn expected_fingerprint_mismatch_stops_without_prompt() {
    let fingerprint = sha(OTHER);
    let (directory, ssh) = target(Some(&fingerprint));
    let refuse = refuse_prompt();
    assert_eq!(trusted(directory.path(), &ssh, Via::Direct, &serve(ED25519), Some(&refuse)), Err(INPUT_MISMATCH.into()));
    assert_eq!(pinned_key(&directory), None);
}

#[test]
fn matching_expected_fingerprint_still_prompts() {
    // 입력한 지문이 맞아도 사용자의 확인 없이 고정하지 않는다. AI CLI가 넣은 지문으로 자동 신뢰되는 경로를 막는다.
    let fingerprint = sha(ED25519);
    let (directory, ssh) = target(Some(&fingerprint));
    let (confirm, shown) = answering(false);
    assert_eq!(trusted(directory.path(), &ssh, Via::Direct, &serve(ED25519), Some(&confirm)), Err(DECLINED.into()));
    assert!(shown.lock().unwrap()[0].ends_with("\n입력한 지문과 일치"));
    assert_eq!(pinned_key(&directory), None);
}

#[test]
fn expected_fingerprint_differs_from_existing_pin_stops() {
    let fingerprint = sha(OTHER);
    let (directory, ssh) = target(Some(&fingerprint));
    pin(&directory, ED25519);
    let refuse = refuse_prompt();
    let error = "등록된 SSH 호스트 키의 지문이 다릅니다".to_owned();
    assert_eq!(trusted(directory.path(), &ssh, Via::Direct, &no_scan, Some(&refuse)), Err(error.clone()));
    assert_eq!(required(directory.path(), &ssh), Err(error));
}

#[test]
fn trusted_pin_blocks_a_changed_server_key() {
    let (directory, ssh) = target(None);
    let (confirm, _) = answering(true);
    trusted(directory.path(), &ssh, Via::Direct, &serve(ED25519), Some(&confirm)).unwrap();
    // 서버 키가 바뀌어도 접속 경로는 창을 띄우거나 핀을 바꾸지 않는다. 접속 계획에는 이전 키만 들어간다.
    let refuse = refuse_prompt();
    let endpoint = endpoint();
    let launch = prepare_saved_with(directory.path(), &ssh, &endpoint, &CancellationToken::new(), Some(&refuse),
        |_, _| Ok(format!("[dev.example.invalid]:2222 {OTHER}")), crate::tunnels::ssm::open_ssh_forward).unwrap();
    let (_, plan) = launch.into_parts();
    let known_hosts = std::fs::read_to_string(plan.config.with_file_name("known_hosts")).unwrap();
    assert!(known_hosts.contains(ED25519) && !known_hosts.contains(OTHER), "{known_hosts}");
    assert_eq!(pinned_key(&directory), Some(sha(ED25519)));
}

#[test]
fn concurrent_first_connects_show_one_prompt() {
    let (directory, ssh) = target(None);
    let prompts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&prompts);
    let confirm: Confirm = Arc::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(100));
        Ok(true)
    });
    let results = std::thread::scope(|scope| {
        let tabs: Vec<_> = (0..2).map(|_| scope.spawn(|| trusted(directory.path(), &ssh, Via::Direct, &serve(ED25519), Some(&confirm))))
            .collect();
        tabs.into_iter().map(|tab| tab.join().unwrap()).collect::<Vec<_>>()
    });
    assert_eq!(results, [Ok(sha(ED25519)), Ok(sha(ED25519))]);
    assert_eq!(prompts.load(Ordering::SeqCst), 1);
}

#[test]
fn missing_pin_without_confirm_reports_unregistered_host() {
    let (directory, ssh) = target(None);
    assert_eq!(trusted(directory.path(), &ssh, Via::Direct, &no_scan, None), Err("호스트 키 확인 필요".into()));
    assert_eq!(required(directory.path(), &ssh), Err("호스트 키 확인 필요".into()));
    // 승격과 원격 화면 SSH 경로가 쓰는 확인 창 없는 접속 준비다.
    assert_eq!(crate::ssh::terminal::saved(directory.path(), &ssh, &CancellationToken::new()).err(), Some("호스트 키 확인 필요".into()));
    assert_eq!(pinned_key(&directory), None);
}

#[test]
fn replace_requires_the_previous_record_and_shows_both_fingerprints() {
    let (directory, ssh) = target(None);
    pin(&directory, ED25519);
    let refuse = refuse_prompt();
    // 같은 키면 창 없이 변경 없음이다.
    assert_eq!(check(directory.path(), &ssh, Via::Direct, &serve(ED25519), &refuse, &refuse), Ok((sha(ED25519), Checked::Unchanged)));

    let (declined, _) = answering(false);
    assert_eq!(check(directory.path(), &ssh, Via::Direct, &serve(OTHER), &refuse, &declined),
        Err("SSH 서버 키 교체가 취소되었습니다".into()));
    assert_eq!(pinned_key(&directory), Some(sha(ED25519)));

    let (replace, shown) = answering(true);
    assert_eq!(check(directory.path(), &ssh, Via::Direct, &serve(OTHER), &refuse, &replace), Ok((sha(OTHER), Checked::Replaced)));
    assert_eq!(shown.lock().unwrap().clone(), [format!(
        "서버: dev.example.invalid\n포트: 2222\n키 종류: ssh-ed25519\n이전 SHA256 지문: {}\n새 SHA256 지문: {}", sha(ED25519), sha(OTHER),
    )]);
    assert_eq!(pinned_key(&directory), Some(sha(OTHER)));

    // 교체는 확인 창에서 본 이전 키가 그대로일 때만 일어난다.
    let endpoint = endpoint();
    let fingerprint = sha(RSA);
    let approved = ApprovedHost::confirm(&endpoint, &endpoint, &fingerprint).unwrap();
    let stale = plain_public_key(ED25519).unwrap();
    assert!(matches!(store(&directory).replace(&approved, &stale, RSA), Err(Error::ExistingPinConflict)));
    assert_eq!(pinned_key(&directory), Some(sha(OTHER)));
}

#[test]
fn check_without_pin_registers_after_trust() {
    let (directory, ssh) = target(None);
    let (confirm, shown) = answering(true);
    let refuse = refuse_prompt();
    assert_eq!(check(directory.path(), &ssh, Via::Direct, &serve(ECDSA), &confirm, &refuse), Ok((sha(ECDSA), Checked::Registered)));
    assert!(shown.lock().unwrap()[0].contains("키 종류: ecdsa-sha2-nistp256\n"));
    assert_eq!(pinned_key(&directory), Some(sha(ECDSA)));
}

#[test]
fn direct_connection_without_pin_trusts_then_prepares_the_plan() {
    let (directory, ssh) = target(None);
    let (confirm, shown) = answering(true);
    let endpoint = endpoint();
    let launch = prepare_saved_with(directory.path(), &ssh, &endpoint, &CancellationToken::new(), Some(&confirm),
        |host, port| {
            assert_eq!((host, port), ("dev.example.invalid", 2222));
            Ok(format!("[{host}]:{port} {ED25519}"))
        }, crate::tunnels::ssm::open_ssh_forward).unwrap();
    assert_eq!(shown.lock().unwrap().len(), 1);
    assert_eq!(pinned_key(&directory), Some(sha(ED25519)));
    let (_, plan) = launch.into_parts();
    assert!(std::fs::read_to_string(plan.config.with_file_name("known_hosts")).unwrap().contains(ED25519));
}

#[test]
fn cancelled_direct_connection_prepares_nothing() {
    let (directory, ssh) = target(None);
    let (confirm, _) = answering(false);
    let endpoint = endpoint();
    let launch = prepare_saved_with(directory.path(), &ssh, &endpoint, &CancellationToken::new(), Some(&confirm),
        |host, port| Ok(format!("[{host}]:{port} {ED25519}")), crate::tunnels::ssm::open_ssh_forward);
    assert_eq!(launch.err(), Some(DECLINED.into()));
    assert_eq!(pinned_key(&directory), None);
}

struct FakeForward;

impl crate::tunnels::Transport for FakeForward {
    fn observe(&mut self) -> crate::tunnels::Observation { crate::tunnels::Observation::Listening }
    fn close(&mut self) -> std::io::Result<()> { Ok(()) }
}

thread_local! {
    static OPENED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SCANNED_PORT: std::cell::Cell<u16> = const { std::cell::Cell::new(0) };
}

fn fake_open(_: &crate::connections::profile::Ssm, port: u16, _: &CancellationToken) -> Result<(u16, FakeForward), String> {
    assert_eq!(port, 2222);
    OPENED.set(OPENED.get() + 1);
    Ok((40022, FakeForward))
}

fn forwarded_scan(host: &str, port: u16) -> Result<String, String> {
    assert_eq!(host, "127.0.0.1");
    SCANNED_PORT.set(port);
    Ok(format!("[{host}]:{port} {ED25519}"))
}

#[test]
fn ssm_without_pin_scans_through_the_forward_after_it_opens() {
    let (directory, mut ssh) = target(None);
    over_ssm(&mut ssh);
    let (confirm, shown) = answering(true);
    // 핀이 없어도 탭은 먼저 열리고, 전달은 PTY 작업자가 연다.
    let opened = OPENED.get();
    let forward = ForwardLaunch::saved_with(directory.path(), &ssh, Some(&confirm), forwarded_scan, fake_open).unwrap().unwrap();
    assert_eq!(OPENED.get(), opened);
    let launch = forward.prepare(&CancellationToken::new()).unwrap();
    assert_eq!(SCANNED_PORT.get(), 40022);
    assert!(shown.lock().unwrap()[0].contains("연결 경로: AWS SSM 경유 · dev-sso · ap-northeast-2 · i-0123456789abcdef0\n"));
    assert_eq!(pinned_key(&directory), Some(sha(ED25519)));
    drop(launch);
}

#[test]
fn ssm_with_pin_is_checked_before_any_aws_call() {
    let fingerprint = sha(OTHER);
    let (directory, mut ssh) = target(Some(&fingerprint));
    over_ssm(&mut ssh);
    pin(&directory, ED25519);
    let refuse = refuse_prompt();
    let opened = OPENED.get();
    let result = ForwardLaunch::saved_with(directory.path(), &ssh, Some(&refuse), no_scan, fake_open);
    assert_eq!(result.err(), Some("등록된 SSH 호스트 키의 지문이 다릅니다".into()));
    assert_eq!(OPENED.get(), opened);
    // 확인 창이 없는 경로는 핀이 없으면 탭을 열기 전에 중단한다.
    let (directory, mut ssh) = target(None);
    over_ssm(&mut ssh);
    assert_eq!(ForwardLaunch::saved_with(directory.path(), &ssh, None, no_scan, fake_open).err(), Some("호스트 키 확인 필요".into()));
}

/// 실제 sshd로 첫 연결 확인, 접속, 키 변경 차단, 교체를 확인한다.
/// `ZZABBA_LIVE_SSH_PHASE=trust`로 핀을 만들고 접속한 뒤, 서버 호스트 키를 바꾸고 `changed`로 다시 실행한다.
/// 나머지 입력: `ZZABBA_LIVE_SSH_DIR`(두 단계가 함께 쓰는 빈 디렉터리), `ZZABBA_LIVE_SSH_PORT`(127.0.0.1의 포트),
/// `ZZABBA_LIVE_SSH_USER`, `ZZABBA_LIVE_SSH_KEY`(개인 키 경로).
#[test]
#[ignore = "실제 sshd 필요"]
fn live_first_connect_trusts_then_blocks_a_changed_key() {
    let var = |name: &str| std::env::var(name).unwrap();
    let directory = std::path::PathBuf::from(var("ZZABBA_LIVE_SSH_DIR"));
    let root = directory.join(TRUST_DIRECTORY);
    if !root.exists() {
        TrustStore::create(&root).unwrap();
    }
    let user = var("ZZABBA_LIVE_SSH_USER");
    let ssh: Ssh = serde_json::from_value(serde_json::json!({
        "host": "127.0.0.1", "port": var("ZZABBA_LIVE_SSH_PORT").parse::<u16>().unwrap(), "account": user,
        "transport": {"kind": "direct"}, "auth": {"kind": "keyFile", "privateKeyPath": var("ZZABBA_LIVE_SSH_KEY")},
    })).unwrap();
    let endpoint = Endpoint::new(&ssh.host, ssh.port).unwrap();
    let whoami = |trust: Option<&Confirm>| -> Result<String, String> {
        let launch = prepare_saved_with(&directory, &ssh, &endpoint, &CancellationToken::new(), trust, keyscan,
            crate::tunnels::ssm::open_ssh_forward)?;
        let (_, plan) = launch.into_parts();
        let mut piped = crate::ssh::terminal::remote_channel(&plan, &["whoami".into()])?.piped;
        let output = piped.stdin(std::process::Stdio::null()).output().map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let refuse = refuse_prompt();
    match var("ZZABBA_LIVE_SSH_PHASE").as_str() {
        "trust" => {
            assert_eq!(whoami(None).unwrap_err(), "호스트 키 확인 필요");
            let (declined, _) = answering(false);
            assert_eq!(whoami(Some(&declined)).unwrap_err(), DECLINED);
            let (confirm, shown) = answering(true);
            assert_eq!(whoami(Some(&confirm)).unwrap(), user);
            let shown = shown.lock().unwrap().clone();
            eprintln!("{}", shown[0]);
            assert!(shown[0].starts_with(&format!("서버: 127.0.0.1\n포트: {}\n키 종류: ssh-ed25519\nSHA256 지문: SHA256:", ssh.port)));
            // 다시 연결할 때는 창을 띄우지 않는다.
            assert_eq!(whoami(Some(&refuse)).unwrap(), user);
        }
        "changed" => {
            let old = pinned(&directory, &ssh).unwrap().unwrap();
            let error = whoami(Some(&refuse)).unwrap_err();
            eprintln!("{error}");
            assert!(error.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") || error.contains("Host key verification failed"), "{error}");
            let (replace, shown) = answering(true);
            let (fingerprint, checked) = check(&directory, &ssh, Via::Direct, &keyscan, &refuse, &replace).unwrap();
            assert_eq!(checked, Checked::Replaced);
            let shown = shown.lock().unwrap().clone();
            eprintln!("{}", shown[0]);
            assert!(shown[0].contains(&format!("이전 SHA256 지문: {old}\n새 SHA256 지문: {fingerprint}")));
            assert_eq!(whoami(Some(&refuse)).unwrap(), user);
        }
        phase => panic!("알 수 없는 단계: {phase}"),
    }
}
