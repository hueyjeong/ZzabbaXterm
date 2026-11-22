use portable_pty::{Child, PtyPair, PtySize};
use std::sync::Arc;

use crate::ssm::{AwsCli, CancellationToken, CliError, Scope, SessionDocument, StartPermit, StartRequest};
use super::{ControlBinding, LiveSsmIdentity};

const START_DIAGNOSTIC_LIMIT: usize = 8 * 1024;

pub(super) struct StartDiagnostic(Option<Vec<u8>>);

impl StartDiagnostic {
    pub(super) fn new() -> Self { Self(Some(Vec::new())) }

    pub(super) fn record(&mut self, bytes: &[u8]) {
        if let Some(output) = &mut self.0 {
            if output.len() + bytes.len() <= START_DIAGNOSTIC_LIMIT {
                output.extend_from_slice(bytes);
            } else {
                self.0 = None;
            }
        }
    }

    pub(super) fn rejection(&self, exit_code: u32) -> Option<String> {
        if exit_code != 254 { return None; }
        crate::ssm::start_rejection(self.0.as_deref()?).map(start_error)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RemoteClose {
    #[default]
    NotStarted,
    Confirmed,
    Unconfirmed,
}

pub(crate) struct Target {
    scope: Scope,
    instance_id: String,
    #[cfg(test)]
    pub(super) fixture: Option<std::path::PathBuf>,
}

impl Target {
    pub(crate) fn new(profile: &str, region: &str, instance_id: &str) -> Result<Self, String> {
        let scope = Scope::new(profile.into(), region.into()).map_err(start_error)?;
        Ok(Self {
            scope,
            instance_id: instance_id.into(),
            #[cfg(test)]
            fixture: None,
        })
    }

    pub(super) async fn prepare_control(&self, binding: &Arc<ControlBinding>) -> Result<Remote, String> {
        let mut remote = self.prepare(Some(binding.identity()), binding.cancel_token()).await?;
        remote.control = Some(Arc::clone(binding));
        Ok(remote)
    }

    pub(super) async fn prepare(&self, central: Option<&LiveSsmIdentity>, cancel: &CancellationToken) -> Result<Remote, String> {
        let document = match central {
            Some(identity) if self.scope.profile() == identity.profile && self.scope.region() == identity.region
                && self.instance_id == identity.instance_id => SessionDocument::AiSharedControl {
                    role: identity.role.as_deref().ok_or_else(|| start_error(CliError::CallerChanged))?,
                    target: &identity.instance_id,
                },
            Some(_) => return Err(start_error(CliError::InvalidTarget)),
            None => SessionDocument::DefaultShell,
        };
        #[cfg(test)]
        if let Some(program) = &self.fixture {
            if central.is_some() { return Err(start_error(CliError::InvalidTarget)); }
            return Ok(Remote::fixture(program));
        }
        let cli = AwsCli::probe(cancel).await.map_err(start_error)?;
        let node = cli.online_nodes(&self.scope, cancel).await.map_err(start_error)?
            .into_iter().find(|node| node.instance_id() == self.instance_id)
            .ok_or_else(|| start_error(CliError::TargetNotConnected))?;
        cli.check_plugin(cancel).await.map_err(start_error)?;
        let request = cli.prepare_document_start(&self.scope, &node, document, cancel).map_err(start_error)?;
        Ok(Remote {
            cli, scope: self.scope.clone(), request, spawned: false,
            control: None,
            #[cfg(test)]
            fixture: None,
            #[cfg(test)]
            is_fixture: false,
        })
    }
}

#[cfg(test)]
#[test]
fn shared_control_rejects_mismatched_target_and_default_shell_fixture() {
    let mut target = Target::new("fixture", "us-west-2", "i-01234567").unwrap();
    let identity = LiveSsmIdentity {
        id: "0".into(), document: 0, account_id: "123456789012".into(),
        role: Some(crate::ssm::AssumedRole::fixture("123456789012")),
        profile: "fixture".into(), region: "us-west-2".into(), instance_id: "i-01234567".into(),
    };
    let invalid = start_error(CliError::InvalidTarget);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let check = |target: &Target, identity: &LiveSsmIdentity| {
        assert_eq!(tauri::async_runtime::block_on(target.prepare(Some(identity), &cancel)).err().as_deref(), Some(invalid.as_str()));
    };
    for change in [
        LiveSsmIdentity { profile: "other".into(), ..identity.clone() },
        LiveSsmIdentity { region: "us-east-1".into(), ..identity.clone() },
        LiveSsmIdentity { instance_id: "i-76543210".into(), ..identity.clone() },
    ] {
        check(&target, &change);
    }
    let no_role = LiveSsmIdentity { role: None, ..identity.clone() };
    assert_eq!(tauri::async_runtime::block_on(target.prepare(Some(&no_role), &cancel)).err(),
        Some(start_error(CliError::CallerChanged)));
    target.fixture = Some(std::path::PathBuf::new());
    check(&target, &identity);
}

pub(super) struct Remote {
    cli: AwsCli,
    scope: Scope,
    request: StartRequest,
    spawned: bool,
    control: Option<Arc<ControlBinding>>,
    #[cfg(test)]
    fixture: Option<StartPermit>,
    #[cfg(test)]
    is_fixture: bool,
}

pub(super) struct SsmIdentity {
    pub(super) account_id: String,
    pub(super) role: Option<std::sync::Arc<crate::ssm::AssumedRole>>,
    pub(super) profile: String,
    pub(super) region: String,
    pub(super) instance_id: String,
}

impl Remote {
    pub(super) fn account_id(&self) -> &str {
        self.request.account_id()
    }

    #[cfg(test)]
    fn fixture(program: &std::path::Path) -> Self {
        let (cli, scope, request, permit) = crate::ssm::fixture_start(program);
        Self { cli, scope, request, spawned: false, control: None, fixture: Some(permit), is_fixture: true }
    }

    pub(super) fn spawn(&mut self, size: PtySize, cancel: &CancellationToken) -> Result<(PtyPair, Box<dyn Child + Send + Sync>), String> {
        let control = self.control.take();
        let cancel = control.as_deref().map_or(cancel, ControlBinding::cancel_token);
        let permit = self.authorize(cancel)?;
        let child = {
            #[cfg(test)]
            if self.is_fixture {
                return self.record_spawn(permit.spawn_fixture(size, cancel, control.as_deref(), || {}, || {}));
            }
            permit.spawn_cancellable(size, cancel, control.as_deref())
        };
        self.record_spawn(child)
    }

    fn record_spawn(&mut self, child: std::io::Result<(PtyPair, Box<dyn Child + Send + Sync>)>) -> Result<(PtyPair, Box<dyn Child + Send + Sync>), String> {
        let child = child.map_err(|error| match error.kind() {
            std::io::ErrorKind::Interrupted => error.to_string(),
            _ => "SSM PTY: 시작 실패".to_owned(),
        })?;
        self.spawned = true;
        Ok(child)
    }

    fn authorize(&mut self, cancel: &CancellationToken) -> Result<StartPermit, String> {
        #[cfg(test)]
        if let Some(permit) = self.fixture.take() { return Ok(permit); }
        tauri::async_runtime::block_on(self.cli.authorize_start(&mut self.request, &self.scope, cancel))
            .map_err(start_error)?.ok_or_else(|| start_error(CliError::Cancelled))
    }

    pub(super) fn wait_connected(&self, cancel: &CancellationToken, work_tab: bool) -> Result<Option<SsmIdentity>, String> {
        tauri::async_runtime::block_on(self.cli.wait_connected(&self.request, &self.scope, cancel))
            .map_err(|_| format!("연결 확인 불가: {}", self.request.target()))?;
        if !work_tab { return Ok(None); }
        #[cfg(test)]
        if self.is_fixture { return Ok(None); }
        Ok(Some(SsmIdentity {
            account_id: self.account_id().into(),
            role: self.request.assumed_role().cloned(),
            profile: self.scope.profile().into(),
            region: self.scope.region().into(),
            instance_id: self.request.target().into(),
        }))
    }

    pub(super) fn close(&mut self) -> (RemoteClose, Option<String>) {
        if !self.spawned {
            return (RemoteClose::NotStarted, None);
        }
        // 문서 취소 토큰은 시작만 중단한다. 종료 조회에는 새 토큰을 사용한다.
        let cancel = CancellationToken::new();
        let confirmed = tauri::async_runtime::block_on(self.cli.close_session(&mut self.request, &self.scope, &cancel)).is_ok();
        (if confirmed { RemoteClose::Confirmed } else { RemoteClose::Unconfirmed }, Some(self.request.target().into()))
    }

    pub(super) fn recheck_closed(&self) -> bool {
        tauri::async_runtime::block_on(self.cli.recheck_closed(&self.request, &self.scope)).is_ok()
    }
}

#[cfg(all(test, any(windows, unix)))]
#[test]
fn submitted_control_start_is_reaped_and_checks_exact_ssm_cleanup_after_revocation() {
    const WAIT: std::time::Duration = std::time::Duration::from_secs(60);
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join(format!("ssm fixture{}", std::env::consts::EXE_SUFFIX));
        let compiled = std::process::Command::new(option_env!("RUSTC").unwrap_or("rustc"))
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/src/terminal/ssm_fixture.rs"))
            .arg("-o").arg(&program).output().unwrap();
        assert!(compiled.status.success(), "{}", String::from_utf8_lossy(&compiled.stderr));
        std::fs::write(root.path().join("mode"), "unknown").unwrap();
        submitted_control_start_shutdown_case(&program, false);
        std::fs::remove_file(root.path().join("History")).unwrap();
        submitted_control_start_shutdown_case(&program, true);
        let _ = done.send(());
    });
    match finished.recv_timeout(WAIT) {
        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => worker.join().unwrap(),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("제어 SSM 종료 확인 대기 시간 초과"),
    }
}

#[cfg(all(test, any(windows, unix)))]
fn submitted_control_start_shutdown_case(program: &std::path::Path, exit_started: bool) {
    use crate::terminal::{ControlExitReport, LocalClose, Sessions, process::Session};
    use std::{sync::mpsc, thread, time::Duration};

    const WAIT: Duration = Duration::from_secs(10);
    let sessions = Sessions::default();
    let central = sessions.insert_live_ssm_fixture("main", 0, "123456789012", "us-west-2", "i-01234567");
    let binding = sessions.bind_ssm_control("main", &central.identity, &CancellationToken::new()).unwrap();
    let cancel = binding.cancel_token();
    let mut remote = Remote::fixture(program);
    let selected = sessions.get("main", &central.identity.id).unwrap();
    let permit = remote.authorize(cancel).unwrap();
    let session = Arc::new(Session::dedicated(&binding));
    let tracked = sessions.track_control(&binding, &session).unwrap();
    let id = tracked.id;
    let transport = crate::ai::control::ControlTransport::from_tracked_fixture(id, Arc::clone(&binding), Arc::clone(&session));
    assert!(sessions.get("main", &id.to_string()).is_err());

    let (pair, mut child) = thread::scope(|scope| {
        let (submit, submitted) = mpsc::channel();
        let (close, closed) = mpsc::channel();
        let closer = scope.spawn(move || {
            submitted.recv_timeout(WAIT).unwrap();
            central.stop();
            close.send(()).unwrap();
        });
        let spawn = permit.spawn_fixture(PtySize::default(), cancel, Some(&binding), || {}, || {
            assert!(binding.registry_unlocked_fixture());
            submit.send(()).unwrap();
            tauri::async_runtime::block_on(async {
                tokio::time::timeout(WAIT, cancel.cancelled()).await.unwrap();
            });
            assert!(matches!(selected.control_submit.try_lock(), Err(std::sync::TryLockError::WouldBlock)));
            assert!(matches!(closed.try_recv(), Err(mpsc::TryRecvError::Empty)));
        });
        let child = remote.record_spawn(spawn).unwrap();
        closed.recv_timeout(WAIT).unwrap();
        closer.join().unwrap();
        child
    });
    assert!(cancel.is_cancelled());
    if exit_started {
        assert_eq!(sessions.drain_controls_for_exit(Duration::ZERO), ControlExitReport {
            pending: 1, ..ControlExitReport::default()
        });
    }
    session.terminate_fixture(child.as_mut(), pair.master.as_ref());
    drop(pair);
    session.cleanup(Some(remote));
    drop(tracked);
    let result = transport.close();
    assert_eq!((result.local, result.remote, result.target.as_deref()),
        (LocalClose::CliReaped, RemoteClose::Unconfirmed, Some("i-01234567")));
    // 정확한 닫기가 첫 완료 보고의 레지스트리 잠금을 앞서 획득한다.
    assert_eq!(sessions.drain_controls_for_exit(WAIT), ControlExitReport {
        remote_unconfirmed: usize::from(exit_started), ..ControlExitReport::default()
    });
    assert!(program.with_file_name("History").is_file());
}

fn start_error(error: CliError) -> String {
    let status = match error {
        CliError::AwsCliMissing => "AWS CLI 없음",
        CliError::AwsCliNotV2 => "AWS CLI v2 필요",
        CliError::InvalidConfiguration if cfg!(target_os = "linux") => "AWS 구성 또는 설치 오류. AWS CLI v2와 Session Manager 플러그인의 관리자 관리 시스템 설치가 필요합니다.",
        CliError::InvalidConfiguration => "AWS 구성 또는 설치 오류",
        CliError::PluginMissing if cfg!(target_os = "linux") => "Session Manager 플러그인 없음. 관리자 관리 시스템 설치가 필요합니다.",
        CliError::PluginMissing => "Session Manager 플러그인 없음",
        CliError::PluginUpgradeRequired { minimum } => {
            return format!("SSM: Session Manager 플러그인 업그레이드 필요 ({minimum} 이상)");
        }
        CliError::TargetNotConnected => "노드 연결 불가",
        CliError::InvalidSessionDocument => "세션 문서 사용 불가",
        CliError::AccessDenied => "권한 거부",
        CliError::SsoLoginRequired => "SSO 재로그인 필요",
        CliError::CredentialsExpired => "자격 증명 만료",
        CliError::Cancelled => "시작 취소",
        CliError::CallerChanged => "AWS 프로필의 신원이 바뀌었습니다",
        _ => return format!("SSM: {error:?}"),
    };
    format!("SSM: {status}")
}
