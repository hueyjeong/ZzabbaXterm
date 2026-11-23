use std::{net::IpAddr, num::NonZeroU64, path::{Component, Path, PathBuf}};

use serde::{Deserialize, Serialize};

pub(super) const VERSION: u32 = 1;
pub(super) type Revision = NonZeroU64;

#[derive(Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct GroupId(String);

#[derive(Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct TargetId(String);

impl GroupId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TargetId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Group {
    pub(super) id: GroupId,
    pub(super) display_name: String,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Target {
    pub(super) id: TargetId,
    pub(super) group_id: GroupId,
    pub(super) display_name: String,
    #[serde(flatten)]
    pub(super) method: Method,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "defaultMethod", rename_all = "camelCase", deny_unknown_fields)]
pub(super) enum Method {
    Local { local: Local },
    Ssm { ssm: Ssm },
    Ssh { ssh: Ssh },
    S3 { s3: S3 },
    Rdp { rdp: Rdp },
    Vnc { vnc: Vnc },
}

pub(crate) enum MethodRef<'a> {
    Local { shell: Shell, start_directory: &'a Path },
    Ssm { profile: &'a str, region: &'a str, instance_id: &'a str },
    Ssh { ssh: &'a Ssh },
    S3 { s3: &'a S3 },
    Desktop { route: &'a DesktopRoute },
}

impl MethodRef<'_> {
    pub(crate) fn uses_tested_account(&self) -> bool {
        matches!(self, Self::Ssm { .. } | Self::S3 { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Shell {
    PowerShell,
    Bash,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Local {
    pub(super) shell: Shell,
    pub(super) start_directory: PathBuf,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Ssm {
    pub(crate) profile: String,
    pub(crate) region: String,
    pub(crate) instance_id: String,
}

impl Ssm {
    fn validate(&self) -> Result<(), ValidationError> {
        use crate::ssm::{valid_instance_id, valid_region, validate_profile};
        validate_profile(&self.profile).map_err(|_| ValidationError::SsmProfile)?;
        if !valid_region(&self.region) {
            return Err(ValidationError::SsmRegion);
        }
        if !valid_instance_id(&self.instance_id) {
            return Err(ValidationError::SsmInstanceId);
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct S3 {
    pub(crate) profile: String,
    pub(crate) region: String,
    pub(crate) bucket: String,
    pub(crate) prefix: String,
}

impl S3 {
    fn validate(&self) -> Result<(), ValidationError> {
        crate::ssm::validate_profile(&self.profile).map_err(|_| ValidationError::S3Profile)?;
        if !crate::ssm::valid_region(&self.region) {
            return Err(ValidationError::S3Region);
        }
        let label = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
        if !(3..=63).contains(&self.bucket.len())
            || !self.bucket.split('.').all(|part| {
                part.bytes().next().is_some_and(label)
                    && part.bytes().last().is_some_and(label)
                    && part.bytes().all(|byte| label(byte) || byte == b'-')
            })
            || self.bucket.parse::<std::net::Ipv4Addr>().is_ok()
            || ["xn--", "sthree-", "amzn-s3-demo-"].iter().any(|prefix| self.bucket.starts_with(prefix))
            || ["--x-s3", "--table-s3", "-s3alias", "--ol-s3", ".mrap"].iter().any(|suffix| self.bucket.ends_with(suffix))
        {
            return Err(ValidationError::S3Bucket);
        }
        if self.prefix.len() > 1024 {
            return Err(ValidationError::S3Prefix);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum DesktopProtocol {
    Rdp,
    Vnc,
}

/// 원격 화면 프로필이다. 비밀번호는 저장하지 않고 연결할 때 서비스 인증에서 받는다.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Rdp {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) tls_only: bool,
    pub(crate) route: DesktopRoute,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Vnc {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) route: DesktopRoute,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub(crate) enum DesktopRoute {
    Direct {},
    /// 저장된 직접 SSH 대상의 서버 루프백(`127.0.0.1:port`)으로 전달한다.
    Ssh { target_id: String },
    /// host가 비면 노드 자신의 포트, 있으면 노드에서 접속하는 원격 호스트로 전달한다.
    Ssm(Ssm),
}

fn desktop_dns(host: &str) -> bool {
    host.parse::<IpAddr>().is_err() && !crate::tunnels::transport::numeric_tail(host) && host.len() <= 253 && host.split('.').all(|label| {
        !label.is_empty() && label.len() <= 63 && !label.starts_with('-') && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

fn validate_desktop(protocol: DesktopProtocol, host: &str, port: u16, route: &DesktopRoute) -> Result<(), ValidationError> {
    if port == 0 {
        return Err(ValidationError::DesktopPort);
    }
    let address = host.parse::<IpAddr>().is_ok() || desktop_dns(host);
    // RDP는 TLS 인증서 이름이 필요하다. 평문 RFB는 터널 끝과 같은 호스트의 서비스만 연다.
    let valid = match (protocol, route) {
        (DesktopProtocol::Rdp, DesktopRoute::Direct {} | DesktopRoute::Ssh { .. }) => desktop_dns(host),
        // SSM 경로도 RDP 인증서 이름이 필요하므로 DNS 이름을 목적지로 쓴다.
        (DesktopProtocol::Rdp, DesktopRoute::Ssm(_)) => desktop_dns(host),
        (DesktopProtocol::Vnc, DesktopRoute::Direct {}) => address,
        (DesktopProtocol::Vnc, DesktopRoute::Ssh { .. } | DesktopRoute::Ssm(_)) => host.is_empty(),
    };
    if !valid {
        return Err(ValidationError::DesktopHost);
    }
    match route {
        DesktopRoute::Direct {} => Ok(()),
        DesktopRoute::Ssh { target_id } if valid_id(target_id) && !target_id.starts_with('-') => Ok(()),
        DesktopRoute::Ssh { .. } => Err(ValidationError::DesktopRoute),
        DesktopRoute::Ssm(target) => target.validate(),
    }
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Ssh {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) account: String,
    pub(crate) host_key_fingerprint: String,
    pub(crate) transport: SshTransport,
    pub(crate) auth: SshAuth,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum SshTransport {
    Direct {},
    Ssm(Ssm),
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub(crate) enum SshAuth {
    Agent { public_key_path: PathBuf },
    KeyFile { private_key_path: PathBuf },
    ManualPassword {},
    StoredPassword { item_id: String },
}

impl Ssh {
    fn validate(&self) -> Result<(), ValidationError> {
        let dns = self.host.strip_suffix('.').unwrap_or(&self.host);
        if self.port == 0 || (self.host.parse::<IpAddr>().is_err() &&
            (dns.len() > 253 || !dns.split('.').all(|label| {
                !label.is_empty() && label.len() <= 63 && !label.starts_with('-') && !label.ends_with('-')
                    && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }))) {
            return Err(ValidationError::SshEndpoint);
        }
        if self.account.len() > 64
            || !self.account.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
            || !self.account.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(ValidationError::SshAccount);
        }
        // 앱의 신뢰 저장소가 실제 서버와 지문을 확인한다. 이곳에서는 참조 형식만 검사한다.
        let Some(digest) = self.host_key_fingerprint.strip_prefix("SHA256:") else {
            return Err(ValidationError::SshHostKeyFingerprint);
        };
        if digest.len() != 43 || !digest.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/".contains(&b))
            || !b"AEIMQUYcgkosw048".contains(&digest.as_bytes()[42])
        {
            return Err(ValidationError::SshHostKeyFingerprint);
        }
        if let SshTransport::Ssm(target) = &self.transport {
            target.validate()?;
        }
        match &self.auth {
            SshAuth::Agent { public_key_path } | SshAuth::KeyFile { private_key_path: public_key_path }
                if !valid_ssh_path(public_key_path) => Err(ValidationError::SshKeyPath),
            SshAuth::StoredPassword { item_id } if !valid_id(item_id) || item_id.starts_with('-') => {
                Err(ValidationError::SshItemId)
            }
            _ => Ok(()),
        }
    }
}

fn valid_ssh_path(path: &Path) -> bool {
    path.is_absolute() && !path.components().any(|part| matches!(part, Component::ParentDir))
        && path.to_str().is_some_and(|value| !value.chars().any(|c| c.is_control() || "\"%$`*?[{".contains(c)))
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Draft {
    version: u32,
    group_revision: Option<Revision>,
    target_revision: Option<Revision>,
    group: Group,
    pub(super) target: Target,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ValidationError {
    Version(u32),
    GroupId,
    TargetId,
    DisplayName,
    GroupMismatch,
    UnsupportedShell,
    StartDirectory,
    SsmProfile,
    SsmRegion,
    SsmInstanceId,
    SshEndpoint,
    SshAccount,
    SshHostKeyFingerprint,
    SshKeyPath,
    SshItemId,
    S3Profile,
    S3Region,
    S3Bucket,
    S3Prefix,
    DesktopHost,
    DesktopPort,
    DesktopRoute,
}

// 구조 검증과 실제 연결 시험은 별개다.
pub(crate) struct ValidatedDraft(Draft);

impl ValidatedDraft {
    pub(crate) fn matches(&self, other: &Self) -> bool {
        self.0 == other.0 && match (self.method(), other.method()) {
            (MethodRef::Local { start_directory: left, .. }, MethodRef::Local { start_directory: right, .. }) => {
                left.as_os_str() == right.as_os_str()
            }
            (MethodRef::Ssh { ssh: left }, MethodRef::Ssh { ssh: right }) => match (&left.auth, &right.auth) {
                (SshAuth::Agent { public_key_path: left }, SshAuth::Agent { public_key_path: right })
                | (SshAuth::KeyFile { private_key_path: left }, SshAuth::KeyFile { private_key_path: right }) => {
                    left.as_os_str() == right.as_os_str()
                }
                _ => true,
            },
            _ => true,
        }
    }
}

impl Draft {
    pub(crate) fn validate(self) -> Result<ValidatedDraft, ValidationError> {
        if self.version != VERSION {
            return Err(ValidationError::Version(self.version));
        }
        self.group.validate()?;
        self.target.validate()?;
        if self.target.group_id != self.group.id {
            return Err(ValidationError::GroupMismatch);
        }
        Ok(ValidatedDraft(self))
    }
}

impl ValidatedDraft {
    pub(super) fn target_id(&self) -> &str {
        self.0.target.id.as_str()
    }
    pub(crate) fn ssh_pin_endpoint(&self, target_id: &str, fingerprint: &str) -> Option<crate::ssh::Endpoint> {
        let MethodRef::Ssh { ssh } = self.method() else { return None; };
        if self.0.target.id.as_str() != target_id || ssh.host_key_fingerprint != fingerprint {
            return None;
        }
        crate::ssh::Endpoint::new(&ssh.host, ssh.port).ok()
    }

    pub(crate) fn method(&self) -> MethodRef<'_> {
        self.0.target.method()
    }

    pub(super) fn into_parts(self) -> (Group, Option<Revision>, Target, Option<Revision>) {
        let Draft { group, group_revision, target, target_revision, .. } = self.0;
        (group, group_revision, target, target_revision)
    }
}

impl Group {
    pub(super) fn validate(&self) -> Result<(), ValidationError> {
        if !valid_id(self.id.as_str()) {
            return Err(ValidationError::GroupId);
        }
        validate_name(&self.display_name)
    }
}

impl Target {
    pub(crate) fn method(&self) -> MethodRef<'_> {
        match &self.method {
            Method::Local { local } => MethodRef::Local { shell: local.shell, start_directory: &local.start_directory },
            Method::Ssm { ssm } => MethodRef::Ssm { profile: &ssm.profile, region: &ssm.region, instance_id: &ssm.instance_id },
            Method::Ssh { ssh } => MethodRef::Ssh { ssh },
            Method::S3 { s3 } => MethodRef::S3 { s3 },
            Method::Rdp { rdp: Rdp { route, .. } } | Method::Vnc { vnc: Vnc { route, .. } } => MethodRef::Desktop { route },
        }
    }

    pub(super) fn validate(&self) -> Result<(), ValidationError> {
        if !valid_id(self.id.as_str()) {
            return Err(ValidationError::TargetId);
        }
        if !valid_id(self.group_id.as_str()) {
            return Err(ValidationError::GroupId);
        }
        validate_name(&self.display_name)?;
        match &self.method {
            Method::Local { local } => {
                if !matches!((cfg!(windows), local.shell), (true, Shell::PowerShell) | (false, Shell::Bash)) {
                    return Err(ValidationError::UnsupportedShell);
                }
                let path = &local.start_directory;
                if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
                    return Err(ValidationError::StartDirectory);
                }
                Ok(())
            }
            Method::Ssm { ssm } => ssm.validate(),
            Method::Ssh { ssh } => ssh.validate(),
            Method::S3 { s3 } => s3.validate(),
            Method::Rdp { rdp } => validate_desktop(DesktopProtocol::Rdp, &rdp.host, rdp.port, &rdp.route),
            Method::Vnc { vnc } => validate_desktop(DesktopProtocol::Vnc, &vnc.host, vnc.port, &vnc.route),
        }
    }
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn validate_name(name: &str) -> Result<(), ValidationError> {
    if name.trim().is_empty() || name.chars().any(char::is_control) {
        return Err(ValidationError::DisplayName);
    }
    Ok(())
}
