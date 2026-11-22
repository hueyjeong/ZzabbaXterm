use portable_pty::{Child, PtyPair, PtySize};

use crate::ssm::{AwsCli, CancellationToken, CliError, Scope, SessionDocument, StartRequest};
use super::LiveSsmIdentity;

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
            let (cli, scope, request, permit) = crate::ssm::fixture_start(program);
            return Ok(Remote { cli, scope, request, spawned: false, fixture: Some(permit), is_fixture: true });
        }
        let cli = AwsCli::probe(cancel).await.map_err(start_error)?;
        let node = cli.online_nodes(&self.scope, cancel).await.map_err(start_error)?
            .into_iter().find(|node| node.instance_id() == self.instance_id)
            .ok_or_else(|| start_error(CliError::TargetNotConnected))?;
        cli.check_plugin(cancel).await.map_err(start_error)?;
        let request = cli.prepare_document_start(&self.scope, &node, document, cancel).map_err(start_error)?;
        Ok(Remote {
            cli, scope: self.scope.clone(), request, spawned: false,
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
    #[cfg(test)]
    fixture: Option<crate::ssm::StartPermit>,
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

    pub(super) fn spawn(&mut self, size: PtySize, cancel: &CancellationToken) -> Result<(PtyPair, Box<dyn Child + Send + Sync>), String> {
        #[cfg(test)]
        if let Some(permit) = self.fixture.take() {
            return self.record_spawn(permit.spawn_fixture(size));
        }
        let permit = tauri::async_runtime::block_on(self.cli.authorize_start(&mut self.request, &self.scope, cancel))
            .map_err(start_error)?.ok_or_else(|| start_error(CliError::Cancelled))?;
        if cancel.is_cancelled() { return Err(start_error(CliError::Cancelled)); }
        self.record_spawn(permit.spawn(size))
    }

    fn record_spawn(&mut self, child: std::io::Result<(PtyPair, Box<dyn Child + Send + Sync>)>) -> Result<(PtyPair, Box<dyn Child + Send + Sync>), String> {
        let child = child.map_err(|_| "SSM PTY: 시작 실패".to_owned())?;
        self.spawned = true;
        Ok(child)
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
