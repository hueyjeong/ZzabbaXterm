use std::fmt::Write;

use sha2::{Digest, Sha256};

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
pub(crate) const APPLICATION: &str = "dev.zzabbaxterm.desktop.s3";
const PREFIX: &str = "dev.zzabbaxterm.desktop.s3:v1:";

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
        .filter(|(access, secret)| !access.chars().chain(secret.chars()).any(char::is_control))
        .ok_or(KeyAccessError::Invalid)?;
    if access_key.is_empty() || secret_key.is_empty() {
        return Err(KeyAccessError::Invalid);
    }
    ServiceKey::new(access_key.into(), secret_key.into())
}

#[cfg(test)]
mod tests;
