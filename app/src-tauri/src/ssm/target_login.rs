//! 저장된 연결 대상의 SSM 경로로 SSO 세션을 확인하고, 실제로 만료된 경우에만 로그인한다.
//! 프로필과 리전은 프런트 인자가 아니라 연결 저장소에서 읽는다.

use std::{collections::HashMap, path::Path, sync::Mutex};

use serde::Serialize;
use tauri::{Manager, Webview};

use super::{AwsCli, CancellationToken, CliError, DirectSsoConfig, Scope};
use crate::{
    connections::{profile::{DesktopRoute, MethodRef, SshTransport}, with_saved_method},
    terminal::remote::start_error,
};

const NOT_SSM: &str = "SSM 경로가 없는 연결 대상입니다";
const IN_PROGRESS: &str = "SSO 로그인 진행 중";
const CANCELLED: &str = "SSO 로그인 취소";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Relogin {
    /// 확인 호출이 성공해 로그인을 실행하지 않았다.
    Valid,
    LoggedIn,
}

/// 대상 ID별 진행 중인 로그인이다. 값은 요청 ID와 취소 토큰이다.
#[derive(Default)]
pub(crate) struct TargetLogins(Mutex<HashMap<String, (String, CancellationToken)>>);

impl TargetLogins {
    fn begin(&self, target_id: &str, request_id: &str) -> Result<CancellationToken, String> {
        let mut logins = self.0.lock().unwrap();
        if logins.contains_key(target_id) {
            return Err(IN_PROGRESS.into());
        }
        let cancel = CancellationToken::new();
        logins.insert(target_id.to_owned(), (request_id.to_owned(), cancel.clone()));
        Ok(cancel)
    }

    fn finish(&self, target_id: &str, request_id: &str) {
        let mut logins = self.0.lock().unwrap();
        if logins.get(target_id).is_some_and(|(id, _)| id == request_id) {
            logins.remove(target_id);
        }
    }

    fn cancel(&self, request_id: &str) {
        let mut logins = self.0.lock().unwrap();
        logins.retain(|_, (id, cancel)| {
            if id != request_id { return true; }
            cancel.cancel();
            false
        });
    }

    /// 기본 창의 문서가 바뀌거나 앱이 끝나면 모든 로그인을 취소한다.
    pub(crate) fn cancel_all(&self) {
        for (_, (_, cancel)) in self.0.lock().unwrap().drain() {
            cancel.cancel();
        }
    }
}

/// SSM 대상, SSH-over-SSM, SSM 경로의 RDP·VNC에서만 범위를 돌려준다.
fn saved_scope(directory: &Path, target_id: &str) -> Result<Scope, String> {
    with_saved_method(directory, target_id, |method, _| {
        let (profile, region) = match method {
            MethodRef::Ssm { profile, region, .. } => (profile, region),
            MethodRef::Ssh { ssh } => match &ssh.transport {
                SshTransport::Ssm(target) => (target.profile.as_str(), target.region.as_str()),
                SshTransport::Direct {} => return Err(NOT_SSM.into()),
            },
            MethodRef::Desktop { route: DesktopRoute::Ssm(target) } => (target.profile.as_str(), target.region.as_str()),
            MethodRef::Desktop { .. } | MethodRef::Local { .. } | MethodRef::S3 { .. } => return Err(NOT_SSM.into()),
        };
        Scope::new(profile.to_owned(), region.to_owned()).map_err(message)
    })
}

fn message(error: CliError) -> String {
    match error {
        CliError::UnsupportedProfile | CliError::SsoLoginNotApplicable => "SSM: 직접 SSO 프로필이 아님".into(),
        CliError::TimedOut => "SSM: SSO 로그인 시간 초과".into(),
        CliError::Cancelled => CANCELLED.into(),
        error => start_error(error),
    }
}

trait Sso {
    fn config(&self, scope: &Scope) -> Result<DirectSsoConfig, CliError>;
    fn check(&self, scope: &Scope, config: &DirectSsoConfig) -> impl Future<Output = Result<(), CliError>> + Send;
    fn login(&self, scope: &Scope, config: &DirectSsoConfig) -> impl Future<Output = Result<(), CliError>> + Send;
}

struct Cli<'a>(&'a CancellationToken);

impl Sso for Cli<'_> {
    fn config(&self, scope: &Scope) -> Result<DirectSsoConfig, CliError> {
        scope.direct_sso_config().map_err(|error| match error {
            CliError::UnsupportedProfile => CliError::SsoLoginNotApplicable,
            error => error,
        })
    }

    async fn check(&self, scope: &Scope, config: &DirectSsoConfig) -> Result<(), CliError> {
        AwsCli::probe(self.0).await?.check_identity(scope, config, self.0).await
    }

    async fn login(&self, scope: &Scope, config: &DirectSsoConfig) -> Result<(), CliError> {
        // 확인 시점의 SSO 설정이 그대로일 때만 로그인 창을 연다.
        AwsCli::probe(self.0).await?.relogin_saved(scope, config, self.0).await
    }
}

async fn relogin_target(directory: &Path, target_id: &str, sso: &impl Sso) -> Result<Relogin, String> {
    let scope = saved_scope(directory, target_id)?;
    let config = sso.config(&scope).map_err(message)?;
    // 로그인 권한은 프런트의 오류 문자열이 아니라 이 확인 결과에서 발급한다.
    match sso.check(&scope, &config).await {
        Ok(()) => Ok(Relogin::Valid),
        Err(CliError::SsoLoginRequired) => sso.login(&scope, &config).await.map(|()| Relogin::LoggedIn).map_err(message),
        Err(error) => Err(message(error)),
    }
}

fn valid_request_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn main_document(webview: &Webview) -> Result<(), String> {
    if webview.label() != "main" || webview.window_ref().label() != "main" {
        return Err("앱의 로컬 기본 창에서만 SSO 로그인을 실행할 수 있습니다".into());
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn ssm_target_relogin(webview: Webview, target_id: String, request_id: String) -> Result<Relogin, String> {
    main_document(&webview)?;
    if !valid_request_id(&request_id) {
        return Err("잘못된 요청 ID".into());
    }
    let directory = webview.path().app_config_dir().map_err(|error| format!("앱 설정 디렉터리: {error}"))?;
    let logins = webview.state::<TargetLogins>();
    let cancel = logins.begin(&target_id, &request_id)?;
    let cli = Cli(&cancel);
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(CANCELLED.into()),
        result = relogin_target(&directory, &target_id, &cli) => result,
    };
    logins.finish(&target_id, &request_id);
    result
}

#[tauri::command]
pub(crate) fn ssm_target_relogin_cancel(webview: Webview, request_id: String) -> Result<(), String> {
    main_document(&webview)?;
    webview.state::<TargetLogins>().cancel(&request_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::{profile::Draft, save_test_profile, tests::{TestDir, desktop_draft, draft, ssh_draft, ssm_draft}};
    use serde_json::{Value, json};
    use std::sync::Mutex;

    struct Fake {
        check: Mutex<Option<Result<(), CliError>>>,
        login: Mutex<Option<Result<(), CliError>>>,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(check: Result<(), CliError>, login: Result<(), CliError>) -> Self {
            Self { check: Mutex::new(Some(check)), login: Mutex::new(Some(login)), calls: Mutex::default() }
        }

        fn record(&self, step: &str, scope: &Scope) {
            self.calls.lock().unwrap().push(format!("{step} {} {}", scope.profile(), scope.region()));
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    fn config() -> DirectSsoConfig {
        DirectSsoConfig {
            account: "123456789012".into(), role: "Dev".into(), region: "us-east-1".into(),
            start_url: "https://example.awsapps.com/start".into(), session: None,
        }
    }

    impl Sso for Fake {
        fn config(&self, scope: &Scope) -> Result<DirectSsoConfig, CliError> {
            self.record("config", scope);
            Ok(config())
        }

        async fn check(&self, scope: &Scope, current: &DirectSsoConfig) -> Result<(), CliError> {
            assert!(*current == config());
            self.record("check", scope);
            self.check.lock().unwrap().take().expect("확인 호출은 한 번만 한다")
        }

        async fn login(&self, scope: &Scope, current: &DirectSsoConfig) -> Result<(), CliError> {
            assert!(*current == config());
            self.record("login", scope);
            self.login.lock().unwrap().take().expect("로그인은 한 번만 한다")
        }
    }

    // 저장소는 기존 그룹에 새 대상을 넣을 때 그룹 리비전을 요구하므로 대상마다 새 그룹을 쓴다.
    fn save(directory: &Path, mut value: Value) {
        let group = format!("group-{}", value["target"]["id"].as_str().unwrap());
        value["group"]["id"] = json!(group);
        value["target"]["groupId"] = json!(group);
        save_test_profile(directory, serde_json::from_value::<Draft>(value).unwrap().validate().unwrap());
    }

    fn run(directory: &Path, id: &str, sso: &Fake) -> Result<Relogin, String> {
        tauri::async_runtime::block_on(relogin_target(directory, id, sso))
    }

    fn saved_targets() -> TestDir {
        let dir = TestDir::new();
        let path = dir.path();
        save(path, ssm_draft(path));
        let route = |profile: &str| json!({"kind": "ssm", "profile": profile, "region": "us-west-2", "instanceId": "i-0123456789abcdef0"});
        let mut ssh = ssh_draft(path);
        ssh["target"]["ssh"]["transport"] = route("ssh-sso");
        ssh["target"]["ssh"]["auth"] = json!({"kind": "agent", "publicKeyPath": path.join("test.pub")});
        save(path, ssh);
        save(path, desktop_draft(path, "rdp", json!({"host": "win.example.test", "port": 3389, "tlsOnly": false, "route": route("rdp-sso")})));
        save(path, desktop_draft(path, "vnc", json!({"host": "", "port": 5900, "route": route("vnc-sso")})));
        let mut direct = ssh_draft(path);
        direct["target"]["id"] = json!("direct-ssh");
        save(path, direct);
        let mut desktop = desktop_draft(path, "vnc", json!({"host": "vnc.example.test", "port": 5900, "route": {"kind": "direct"}}));
        desktop["target"]["id"] = json!("direct-vnc");
        save(path, desktop);
        save(path, draft(path, "work", "local-shell"));
        dir
    }

    #[test]
    fn saved_target_id_selects_ssm_scope_and_logs_in_only_when_expired() {
        let dir = saved_targets();
        for (id, scope) in [
            ("ssm-node", "company-sso ap-northeast-2"),
            ("ssh-server", "ssh-sso us-west-2"),
            ("rdp-desktop", "rdp-sso us-west-2"),
            ("vnc-desktop", "vnc-sso us-west-2"),
        ] {
            let fake = Fake::new(Err(CliError::SsoLoginRequired), Ok(()));
            assert_eq!(run(dir.path(), id, &fake), Ok(Relogin::LoggedIn), "{id}");
            assert_eq!(fake.calls(), [format!("config {scope}"), format!("check {scope}"), format!("login {scope}")], "{id}");
        }
    }

    #[test]
    fn login_runs_only_when_check_reports_sso_login_required() {
        let dir = saved_targets();
        let valid = Fake::new(Ok(()), Ok(()));
        assert_eq!(run(dir.path(), "ssm-node", &valid), Ok(Relogin::Valid));
        assert_eq!(valid.calls(), ["config company-sso ap-northeast-2", "check company-sso ap-northeast-2"]);
        for error in [CliError::AccessDenied, CliError::CredentialsExpired, CliError::TimedOut, CliError::EndpointUnavailable] {
            let fake = Fake::new(Err(error), Ok(()));
            assert_eq!(run(dir.path(), "ssm-node", &fake), Err(message(error)));
            assert!(!fake.calls().iter().any(|call| call.starts_with("login")));
        }
        let failed = Fake::new(Err(CliError::SsoLoginRequired), Err(CliError::CallerChanged));
        assert_eq!(run(dir.path(), "ssm-node", &failed), Err(start_error(CliError::CallerChanged)));
    }

    #[test]
    fn targets_without_ssm_route_are_rejected_before_check() {
        let dir = saved_targets();
        for id in ["direct-ssh", "direct-vnc", "local-shell"] {
            let fake = Fake::new(Ok(()), Ok(()));
            assert_eq!(run(dir.path(), id, &fake), Err(NOT_SSM.into()), "{id}");
            assert!(fake.calls().is_empty(), "{id}");
        }
        let fake = Fake::new(Ok(()), Ok(()));
        assert_eq!(run(dir.path(), "missing", &fake), Err("저장된 연결 대상을 찾을 수 없습니다".into()));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn one_login_per_target_and_cancel_by_request_id() {
        let logins = TargetLogins::default();
        let first = logins.begin("ssm-node", "a").unwrap();
        assert_eq!(logins.begin("ssm-node", "b").unwrap_err(), IN_PROGRESS);
        let other = logins.begin("rdp-desktop", "c").unwrap();
        logins.cancel("b");
        assert!(!first.is_cancelled());
        logins.finish("ssm-node", "b");
        assert_eq!(logins.begin("ssm-node", "d").unwrap_err(), IN_PROGRESS);
        logins.cancel("a");
        assert!(first.is_cancelled() && !other.is_cancelled());
        let again = logins.begin("ssm-node", "e").unwrap();
        logins.cancel_all();
        assert!(again.is_cancelled() && other.is_cancelled());
        assert!(logins.begin("rdp-desktop", "f").is_ok());
        assert!(valid_request_id("0b7c-41") && !valid_request_id("") && !valid_request_id("a b") && !valid_request_id(&"a".repeat(65)));
    }

    #[test]
    fn start_error_matches_frontend_sso_marker() {
        // app/src/features/ssm/ssoLogin.ts의 SSO_LOGIN_REQUIRED와 같은 값이어야 한다.
        assert_eq!(start_error(CliError::SsoLoginRequired), "SSM: SSO 재로그인 필요");
    }
}
