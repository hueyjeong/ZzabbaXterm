use std::{
    ffi::OsString,
    io::{self, Read, Write},
};

use zeroize::Zeroizing;

use super::{ACK, BROKER_ENV, ENDPOINT_ENV, MAX_PASSWORD, peer};

#[cfg(target_os = "linux")]
#[path = "transport_linux.rs"]
mod os;
#[cfg(windows)]
#[path = "transport_windows.rs"]
mod os;

pub(super) use os::Listener;

fn address_id() -> io::Result<String> {
    use std::fmt::Write;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| io::ErrorKind::Other)?;
    let mut id = String::with_capacity(32);
    for b in random {
        write!(id, "{b:02x}").expect("writing to a String cannot fail");
    }
    Ok(id)
}

pub(super) fn run_helper() -> io::Result<()> {
    if std::env::var_os("SSH_ASKPASS_PROMPT").is_some()
        || std::env::var_os("BW_SESSION").is_some()
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    // sudo가 실행한 askpass는 호출 사용자 신원으로 돌고 setuid 부모의 환경을 물려받는다.
    let sudo_mode = std::env::var_os(super::sudo::MODE_ENV).as_deref() == Some(super::sudo::SUDO_MODE.as_ref());
    let requested = if sudo_mode {
        if std::env::var_os("SSH_ASKPASS").is_some()
            || std::env::var_os("SSH_ASKPASS_REQUIRE").is_some()
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        std::env::var_os("SUDO_ASKPASS").ok_or(io::ErrorKind::InvalidInput)?
    } else {
        if std::env::var_os("SSH_ASKPASS_REQUIRE").as_deref() != Some("force".as_ref())
            || std::env::var_os("SUDO_ASKPASS").is_some()
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        std::env::var_os("SSH_ASKPASS").ok_or(io::ErrorKind::InvalidInput)?
    };
    let endpoint: OsString = std::env::var_os(ENDPOINT_ENV).ok_or(io::ErrorKind::NotFound)?;
    let broker = std::env::var(BROKER_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or(io::ErrorKind::InvalidInput)?;
    if std::path::Path::new(&requested).canonicalize()?
        != std::env::current_exe()?.canonicalize()?
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    // 응답에는 검증할 원본 핸들의 복제본만 사용한다.
    #[cfg(target_os = "linux")]
    use std::os::fd::AsFd;
    #[cfg(windows)]
    use std::os::windows::io::AsHandle;
    #[cfg(target_os = "linux")]
    let mut output = std::fs::File::from(io::stdout().as_fd().try_clone_to_owned()?);
    #[cfg(windows)]
    let mut output = std::fs::File::from(io::stdout().as_handle().try_clone_to_owned()?);
    let proof = peer::stdout_proof(&output)?;
    let mut pipe = os::connect(&endpoint, broker)?;
    pipe.write_all(&proof.to_be_bytes())?;
    // 출력 증명이 성공하기 전에는 브로커가 금고를 열지 않는다.
    let mut len = [0; 2];
    pipe.read_exact(&mut len)?;
    let len = u16::from_be_bytes(len) as usize;
    if !(1..=MAX_PASSWORD).contains(&len) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut password = Zeroizing::new([0; MAX_PASSWORD + 1]);
    pipe.read_exact(&mut password[..len])?;
    password[len] = b'\n';
    output.write_all(&password[..=len])?;
    drop(password);
    pipe.write_all(&[ACK])
}

#[cfg(target_os = "linux")]
const HELPER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(31);
