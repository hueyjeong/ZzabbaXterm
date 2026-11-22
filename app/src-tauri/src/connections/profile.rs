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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Target {
    pub(super) id: TargetId,
    pub(super) group_id: GroupId,
    pub(super) display_name: String,
    pub(super) default_method: MethodId,
    pub(super) local: Local,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MethodId {
    Local,
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

#[derive(Debug, Deserialize)]
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
}

// 구조 검증 결과이며, 실제 셸 시험 결과는 상위 로컬 명령에서 관리한다.
pub(crate) struct ValidatedDraft(Draft);

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
    pub(crate) fn shell(&self) -> Shell {
        self.0.target.local.shell
    }

    pub(crate) fn start_directory(&self) -> &Path {
        &self.0.target.local.start_directory
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
    pub(super) fn validate(&self) -> Result<(), ValidationError> {
        if !valid_id(self.id.as_str()) {
            return Err(ValidationError::TargetId);
        }
        if !valid_id(self.group_id.as_str()) {
            return Err(ValidationError::GroupId);
        }
        validate_name(&self.display_name)?;
        self.validate_local()
    }

    fn validate_local(&self) -> Result<(), ValidationError> {
        if !matches!((cfg!(windows), self.local.shell), (true, Shell::PowerShell) | (false, Shell::Bash)) {
            return Err(ValidationError::UnsupportedShell);
        }
        let path = &self.local.start_directory;
        if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
            return Err(ValidationError::StartDirectory);
        }
        Ok(())
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
