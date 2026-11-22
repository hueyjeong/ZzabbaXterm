use std::{ptr::null_mut, slice};

use windows_sys::Win32::{
    Foundation::{ERROR_CANCELLED, ERROR_NOT_FOUND, GetLastError},
    Security::Credentials::{CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW},
};
use zeroize::Zeroize;

use super::{KeyAccessError, ServiceKey, decode};

struct Credential(*mut CREDENTIALW);

impl Drop for Credential {
    fn drop(&mut self) {
        // CredReadW의 호출자 소유 버퍼는 CredFree로 반환한다.
        unsafe {
            let value = &mut *self.0;
            if !value.CredentialBlob.is_null() {
                slice::from_raw_parts_mut(value.CredentialBlob, value.CredentialBlobSize as usize)
                    .zeroize();
            }
            CredFree(self.0.cast());
        }
    }
}

pub(super) fn read(target: &str) -> Result<ServiceKey, KeyAccessError> {
    let target: Vec<u16> = target.encode_utf16().chain([0]).collect();
    let mut credential = null_mut();
    // 대상 이름과 Generic 형식을 지정해 단일 항목만 읽는다.
    unsafe {
        if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) == 0 {
            return Err(match GetLastError() {
                ERROR_NOT_FOUND => KeyAccessError::Missing,
                ERROR_CANCELLED => KeyAccessError::Cancelled,
                _ => KeyAccessError::Unavailable,
            });
        }
        let credential = Credential(credential);
        let value = &*credential.0;
        if value.CredentialBlob.is_null() {
            return Err(KeyAccessError::Invalid);
        }
        decode(slice::from_raw_parts(
            value.CredentialBlob,
            value.CredentialBlobSize as usize,
        ))
    }
}
