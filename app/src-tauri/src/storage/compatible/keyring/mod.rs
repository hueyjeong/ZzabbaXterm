use std::fmt::Write;

use sha2::{Digest, Sha256};
#[cfg(any(windows, target_os = "linux", test))]
use zeroize::Zeroizing;

use super::{KeyAccessError, ServiceKey, ServiceKeyScope, opaque_id};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// `CompatibleService::connect`의 키 읽기 콜백에서 호출한다.
/// `approve`는 네이티브 승인 경계에서 확인한 이 서비스와 항목의 승인 여부를 반환한다.
/// Windows는 이 이름의 Generic Credential을, Linux는 `application`과 `target` 속성이
/// 각각 `APPLICATION`과 이 이름인 Secret Service 항목을 사용한다.
/// 두 금고의 값은 UTF-8 액세스 키 ID, LF, UTF-8 비밀 키 순서다.
pub(crate) fn read_approved(
    scope: &ServiceKeyScope,
    item_id: &str,
    approve: impl FnOnce(&ServiceKeyScope, &str) -> bool,
) -> Result<ServiceKey, KeyAccessError> {
    read_with(scope, item_id, approve, read_os)
}

#[cfg(target_os = "linux")]
pub(crate) const APPLICATION: &str = "dev.zxt.desktop.s3";
const PREFIX: &str = "dev.zxt.desktop.s3:v1:";

/// 제품 등록 경계에서도 같은 이름을 사용한다. 해시 입력은 비밀이 아닌 대상과 항목 ID뿐이다.
pub(crate) fn target_name(
    scope: &ServiceKeyScope,
    item_id: &str,
) -> Result<String, KeyAccessError> {
    if !opaque_id(item_id) {
        return Err(KeyAccessError::Invalid);
    }
    let mut digest = Sha256::new();
    digest.update(scope.service_id());
    digest.update([0]);
    digest.update(scope.origin());
    digest.update([0]);
    digest.update(item_id);
    let mut target = String::with_capacity(PREFIX.len() + 64);
    target.push_str(PREFIX);
    for byte in digest.finalize() {
        write!(target, "{byte:02x}").expect("writing to String is infallible");
    }
    Ok(target)
}

fn read_with(
    scope: &ServiceKeyScope,
    item_id: &str,
    approve: impl FnOnce(&ServiceKeyScope, &str) -> bool,
    read: impl FnOnce(&str) -> Result<ServiceKey, KeyAccessError>,
) -> Result<ServiceKey, KeyAccessError> {
    let target = target_name(scope, item_id)?;
    if !approve(scope, item_id) {
        return Err(KeyAccessError::Cancelled);
    }
    read(&target)
}

#[cfg(any(windows, target_os = "linux", test))]
fn encode_payload(access: &str, secret: &str) -> Result<Zeroizing<Vec<u8>>, KeyAccessError> {
    let len = access.len().saturating_add(secret.len()).saturating_add(1);
    if len > 2_560 || !valid_payload_fields(access, secret) {
        return Err(KeyAccessError::Invalid);
    }
    let mut payload = Zeroizing::new(Vec::with_capacity(len));
    payload.extend_from_slice(access.as_bytes());
    payload.push(b'\n');
    payload.extend_from_slice(secret.as_bytes());
    Ok(payload)
}

#[cfg(target_os = "linux")]
pub(crate) use linux::{prompt, write_new};
#[cfg(windows)]
pub(crate) use windows::{prompt, write_new};

fn valid_payload_fields(access: &str, secret: &str) -> bool {
    !access.is_empty() && !secret.is_empty() && !access.chars().chain(secret.chars()).any(char::is_control)
}

#[cfg(target_os = "linux")]
use linux::read as read_os;
#[cfg(windows)]
use windows::read as read_os;
#[cfg(not(any(target_os = "linux", windows)))]
fn read_os(_: &str) -> Result<ServiceKey, KeyAccessError> {
    Err(KeyAccessError::Unavailable)
}

fn decode(payload: &[u8]) -> Result<ServiceKey, KeyAccessError> {
    let (access_key, secret_key) = std::str::from_utf8(payload)
        .ok()
        .and_then(|text| text.split_once('\n'))
        .filter(|(access, secret)| valid_payload_fields(access, secret))
        .ok_or(KeyAccessError::Invalid)?;
    ServiceKey::new(access_key.into(), secret_key.into())
}

#[cfg(test)]
mod tests;
