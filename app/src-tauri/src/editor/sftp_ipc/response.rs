use russh_sftp::{client::error::Error as ProtocolError, protocol::StatusCode};
use serde::Serialize;

use crate::editor::sftp;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(super) enum Error {
    DocumentClosed,
    InvalidHandle,
    InvalidEntry,
    Unavailable,
    Disconnected,
    TimedOut,
    NotRegular,
    NotUtf8,
    LimitExceeded,
    PathChanged,
    ReadChanged,
    Missing,
    PermissionDenied,
    AtomicReplaceUnavailable,
    ReopenRequired,
    RemoteFailure,
}

impl From<sftp::Error> for Error {
    fn from(error: sftp::Error) -> Self {
        Self::from(&error)
    }
}

impl From<&sftp::Error> for Error {
    fn from(error: &sftp::Error) -> Self {
        use sftp::Error as Core;
        // 서버가 보낸 메시지와 로컬 SSH 진단 대신 고정된 코드만 노출한다.
        match error {
            Core::Prepare { cause, .. } => Self::from(cause.as_ref()),
            Core::Terminal(_) | Core::Unavailable | Core::Spawn(_) | Core::Random(_) => Self::Unavailable,
            Core::Disconnected => Self::Disconnected,
            Core::TimedOut | Core::Protocol(ProtocolError::Timeout) => Self::TimedOut,
            Core::NotRegular => Self::NotRegular,
            Core::InvalidUtf8 => Self::NotUtf8,
            Core::LimitExceeded | Core::Protocol(ProtocolError::Limited(_)) => Self::LimitExceeded,
            Core::PathChanged => Self::PathChanged,
            Core::ReadChanged => Self::ReadChanged,
            Core::AtomicReplaceUnavailable => Self::AtomicReplaceUnavailable,
            Core::ReopenRequired => Self::ReopenRequired,
            Core::Protocol(ProtocolError::Status(status)) => match status.status_code {
                StatusCode::NoSuchFile => Self::Missing,
                StatusCode::PermissionDenied => Self::PermissionDenied,
                StatusCode::NoConnection | StatusCode::ConnectionLost => Self::Disconnected,
                _ => Self::RemoteFailure,
            },
            _ => Self::RemoteFailure,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(super) enum RemoteVersion {
    Text { text: String },
    Missing,
    NotRegular,
    NotUtf8,
    TooLarge,
    DirectoryChanged,
    ReadChanged,
}

impl From<sftp::RemoteVersion> for RemoteVersion {
    fn from(remote: sftp::RemoteVersion) -> Self {
        use sftp::RemoteVersion as Core;
        match remote {
            Core::Text(text) => Self::Text { text },
            Core::Missing => Self::Missing,
            Core::NotRegular => Self::NotRegular,
            Core::NotUtf8 => Self::NotUtf8,
            Core::TooLarge => Self::TooLarge,
            Core::DirectoryChanged => Self::DirectoryChanged,
            Core::ReadChanged => Self::ReadChanged,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum SaveStage {
    Prepare,
    Create,
    Write,
    Permissions,
    Close,
    Recheck,
    Replace,
}

impl From<sftp::SaveStage> for SaveStage {
    fn from(stage: sftp::SaveStage) -> Self {
        use sftp::SaveStage as Core;
        match stage {
            Core::Prepare => Self::Prepare,
            Core::Create => Self::Create,
            Core::Write => Self::Write,
            Core::Permissions => Self::Permissions,
            Core::Close => Self::Close,
            Core::Recheck => Self::Recheck,
            Core::Replace => Self::Replace,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(super) enum Cleanup {
    Removed,
    Failed { error: Error },
    SkippedDirectoryChanged,
}

impl From<sftp::Cleanup> for Cleanup {
    fn from(cleanup: sftp::Cleanup) -> Self {
        match cleanup {
            sftp::Cleanup::Removed => Self::Removed,
            sftp::Cleanup::Failed(error) => Self::Failed { error: error.into() },
            sftp::Cleanup::SkippedDirectoryChanged => Self::SkippedDirectoryChanged,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub(super) enum SaveResult {
    Saved,
    Conflict {
        remote: RemoteVersion,
        #[serde(skip_serializing_if = "Option::is_none")]
        cleanup: Option<Cleanup>,
    },
    Unknown {
        error: Error,
        #[serde(rename = "recoveryPath", skip_serializing_if = "Option::is_none")]
        recovery_path: Option<String>,
    },
    Failed {
        stage: SaveStage,
        error: Error,
        #[serde(skip_serializing_if = "Option::is_none")]
        cleanup: Option<Cleanup>,
    },
}

impl SaveResult {
    pub(super) fn failed(stage: SaveStage, error: Error) -> Self {
        Self::Failed { stage, error, cleanup: None }
    }
}

impl From<sftp::SaveOutcome> for SaveResult {
    fn from(outcome: sftp::SaveOutcome) -> Self {
        match outcome {
            sftp::SaveOutcome::Saved => Self::Saved,
            sftp::SaveOutcome::Conflict { remote, cleanup } => Self::Conflict { remote: remote.into(), cleanup: Some(cleanup.into()) },
            sftp::SaveOutcome::Unconfirmed(error) => Self::Unknown { error: error.into(), recovery_path: None },
            sftp::SaveOutcome::Failed { stage, error, cleanup } => Self::Failed {
                stage: stage.into(), error: error.into(), cleanup: cleanup.map(Into::into),
            },
        }
    }
}
