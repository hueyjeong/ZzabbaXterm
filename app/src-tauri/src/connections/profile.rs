use std::{num::NonZeroU64, path::{Path, PathBuf}};

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
}

pub(crate) enum MethodRef<'a> {
    Local { shell: Shell, start_directory: &'a Path },
    Ssm { profile: &'a str, region: &'a str, instance_id: &'a str },
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
pub(super) struct Ssm {
    profile: String,
    region: String,
    instance_id: String,
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

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Draft {
    version: u32,
    group_revision: Option<Revision>,
    target_revision: Option<Revision>,
    group: Group,
    target: Target,
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
}

// 구조 검증과 실제 연결 시험은 별개다.
pub(crate) struct ValidatedDraft(Draft);

impl ValidatedDraft {
    pub(super) fn matches(&self, other: &Self) -> bool {
        self.0 == other.0 && match (self.method(), other.method()) {
            (MethodRef::Local { start_directory: left, .. }, MethodRef::Local { start_directory: right, .. }) => {
                left.as_os_str() == right.as_os_str()
            }
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
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn validate_name(name: &str) -> Result<(), ValidationError> {
    if name.trim().is_empty() || name.chars().any(char::is_control) {
        return Err(ValidationError::DisplayName);
    }
    Ok(())
}
