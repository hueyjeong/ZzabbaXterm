use super::{Captured, CliError, decode};
use chrono::{DateTime, FixedOffset};
use serde::Deserialize;
use std::{ffi::OsStr, time::SystemTime};
use zeroize::Zeroizing;

const NULL_CONFIG: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Export {
    version: u32,
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
    expiration: String,
}

impl Drop for Export {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.access_key_id.zeroize();
        self.secret_access_key.zeroize();
        self.session_token.zeroize();
    }
}

pub(super) struct Credentials {
    export: Export,
    expires: SystemTime,
    home: super::home::CliHome,
}

impl Credentials {
    pub(super) fn parse(captured: Captured) -> Result<Self, CliError> {
        let bytes = Zeroizing::new(captured.success()?);
        let export: Export = decode(&bytes)?;
        let expires = DateTime::<FixedOffset>::parse_from_rfc3339(&export.expiration)
            .map_err(|_| CliError::InvalidResponse)?
            .into();
        if export.version != 1
            || [
                &export.access_key_id,
                &export.secret_access_key,
                &export.session_token,
            ]
            .into_iter()
            .any(|value| value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_graphic()))
        {
            return Err(CliError::InvalidResponse);
        }
        let credentials = Self {
            export,
            expires,
            home: super::home::CliHome::new()?,
        };
        credentials.require_current()?;
        Ok(credentials)
    }

    #[cfg(test)]
    pub(super) fn expire_for_test(&mut self) {
        self.expires = SystemTime::UNIX_EPOCH;
    }

    pub(super) fn expires_after(&self, other: &Self) -> bool {
        self.expires > other.expires
    }

    #[cfg(test)]
    pub(super) fn expire_in_for_test(&mut self, after: std::time::Duration) {
        self.expires = SystemTime::now() + after;
    }

    /// 남은 수명이 `margin`보다 짧으면 참이다.
    pub(super) fn expires_within(&self, margin: std::time::Duration) -> bool {
        self.expires <= SystemTime::now() + margin
    }

    pub(super) fn require_current(&self) -> Result<(), CliError> {
        if self.expires <= SystemTime::now() {
            return Err(CliError::CredentialsExpired);
        }
        Ok(())
    }

    pub(super) fn home(&self) -> &super::home::CliHome {
        &self.home
    }

    // AWS CLI의 명시적 --profile은 환경 자격 공급자를 끈다. 호출자는 Scope만 유지한다.
    pub(super) fn env(&self, region: &str, mut set: impl FnMut(&str, &OsStr)) {
        self.home.env(&mut set);
        for (key, value) in [
            ("AWS_ACCESS_KEY_ID", self.export.access_key_id.as_str()),
            ("AWS_SECRET_ACCESS_KEY", &self.export.secret_access_key),
            ("AWS_SESSION_TOKEN", &self.export.session_token),
            ("AWS_CONFIG_FILE", NULL_CONFIG),
            ("AWS_SHARED_CREDENTIALS_FILE", NULL_CONFIG),
            ("AWS_DEFAULT_REGION", region),
            ("AWS_REGION", region),
        ] {
            set(key, OsStr::new(value));
        }
    }
}

pub(super) fn export_env(config: &std::path::Path, mut set: impl FnMut(&str, &OsStr)) {
    set("AWS_CONFIG_FILE", config.as_os_str());
    set("AWS_SHARED_CREDENTIALS_FILE", OsStr::new(NULL_CONFIG));
}
