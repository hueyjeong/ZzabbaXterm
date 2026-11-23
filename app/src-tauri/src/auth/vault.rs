use std::{io, path::Path, process::Stdio};

use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use zeroize::Zeroizing;

use super::{Error, MAX_PASSWORD, peer::TrustedFile};

const BW: &str = if cfg!(windows) { "bw.exe" } else { "bw" };

pub struct Bitwarden {
    program: TrustedFile,
    session: Option<Zeroizing<String>>,
}

impl Bitwarden {
    /// 관리자가 검증하여 설치한 실행기의 절대 경로와 네이티브에서 얻은 선택적 세션이다.
    pub fn new(path: &Path, session: Option<Zeroizing<String>>) -> Result<Self, Error> {
        if !path.is_absolute()
            || path
                .file_name()
                .is_none_or(|name| !name.eq_ignore_ascii_case(BW))
        {
            return Err(Error::UntrustedExecutable);
        }
        match path.symlink_metadata() {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::VaultUnavailable),
            Err(_) => return Err(Error::UntrustedExecutable),
            Ok(_) => (),
        }
        Ok(Self {
            program: TrustedFile::check(path, BW)?,
            session,
        })
    }

    pub(super) async fn password(&self, item: &str) -> Result<Zeroizing<Vec<u8>>, Error> {
        let (ok, status, _) = self.run(&["status"], 8192).await?;
        if !ok {
            return Err(Error::VaultFailed);
        }
        #[derive(Deserialize)]
        struct Status<'a> {
            #[serde(borrow)]
            status: &'a str,
        }
        match serde_json::from_slice::<Status<'_>>(&status)
            .map_err(|_| Error::VaultFailed)?
            .status
        {
            "locked" => return Err(Error::VaultLocked),
            "unauthenticated" => return Err(Error::VaultUnauthenticated),
            "unlocked" => (),
            _ => return Err(Error::VaultFailed),
        }
        let (ok, password, error) = self.run(&["get", "password", item], MAX_PASSWORD).await?;
        if !ok {
            return Err(match error.trim_ascii() {
                b"Not found." => Error::ItemMissing,
                b"Vault is locked." => Error::VaultLocked,
                b"You are not logged in." => Error::VaultUnauthenticated,
                _ => Error::VaultFailed,
            });
        }
        if password.is_empty() || password.iter().any(|b| matches!(b, 0 | b'\r' | b'\n')) {
            return Err(Error::InvalidPassword);
        }
        Ok(password)
    }

    async fn run(&self, args: &[&str], limit: usize) -> Result<(bool, Secret, Secret), Error> {
        let mut command = Command::new(self.program.path());
        command
            .current_dir(
                self.program
                    .path()
                    .parent()
                    .ok_or(Error::UntrustedExecutable)?,
            )
            .env_clear()
            .envs(
                [
                    "HOME",
                    "USERPROFILE",
                    "APPDATA",
                    "LOCALAPPDATA",
                    "SYSTEMROOT",
                    "WINDIR",
                ]
                .into_iter()
                .filter_map(|k| std::env::var_os(k).map(|v| (k, v))),
            )
            .envs(
                self.session
                    .as_ref()
                    .map(|key| ("BW_SESSION", key.as_str())),
            )
            .env("NO_COLOR", "1")
            .args(["--raw", "--nointeraction"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        let mut child = command.spawn().map_err(|_| Error::VaultUnavailable)?;
        let stdout = child.stdout.take().ok_or(Error::VaultFailed)?;
        let stderr = child.stderr.take().ok_or(Error::VaultFailed)?;
        let (stdout, stderr, status) =
            tokio::try_join!(capture(stdout, limit), capture(stderr, 8192), async {
                child.wait().await.map_err(|_| Error::VaultFailed)
            },)?;
        Ok((status.success(), stdout, stderr))
    }

    #[cfg(test)]
    pub(super) fn fixture(program: TrustedFile, session: Zeroizing<String>) -> Self {
        Self {
            program,
            session: Some(session),
        }
    }
}

type Secret = Zeroizing<Vec<u8>>;

async fn capture(mut pipe: impl AsyncRead + Unpin, limit: usize) -> Result<Secret, Error> {
    // 읽은 조각을 재할당하며 남기지 않도록 최대 크기와 초과 확인용 한 바이트를 함께 잡는다.
    let mut output = Zeroizing::new(vec![0; limit + 1]);
    let mut len = 0;
    while len <= limit {
        let count = pipe
            .read(&mut output[len..])
            .await
            .map_err(|_| Error::VaultFailed)?;
        if count == 0 {
            output.truncate(len);
            return Ok(output);
        }
        len += count;
    }
    Err(Error::VaultFailed)
}

pub(super) fn valid_item(item: &str) -> bool {
    item.len() == 36
        && item.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
