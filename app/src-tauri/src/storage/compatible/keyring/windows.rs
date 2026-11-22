use std::{ptr::null_mut, slice, sync::Mutex};

use windows_sys::Win32::{
    Foundation::{ERROR_CANCELLED, ERROR_NOT_FOUND, GetLastError},
    Security::Credentials::{
        CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
        CREDENTIALW, CredFree, CredReadW, CredWriteW,
    },
};
use zeroize::Zeroize;

use super::{KeyAccessError, ServiceKey, decode};

mod enrollment;
pub(crate) use enrollment::prompt;

const MAX_FIELD_UNITS: usize = 256;
static WRITES: Mutex<()> = Mutex::new(());

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

fn read_raw(target: &[u16]) -> Result<Credential, KeyAccessError> {
    let mut credential = null_mut();
    if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } != 0 {
        return Ok(Credential(credential));
    }
    Err(match unsafe { GetLastError() } {
        ERROR_NOT_FOUND => KeyAccessError::Missing,
        ERROR_CANCELLED => KeyAccessError::Cancelled,
        _ => KeyAccessError::Unavailable,
    })
}

impl Credential {
    fn blob(&self) -> Result<&[u8], KeyAccessError> {
        let value = unsafe { &*self.0 };
        if value.CredentialBlob.is_null() {
            return Err(KeyAccessError::Invalid);
        }
        Ok(unsafe {
            slice::from_raw_parts(value.CredentialBlob, value.CredentialBlobSize as usize)
        })
    }
}

pub(super) fn read(target: &str) -> Result<ServiceKey, KeyAccessError> {
    let target: Vec<u16> = target.encode_utf16().chain([0]).collect();
    decode(read_raw(&target)?.blob()?)
}

pub(crate) fn write_new(target: &str, label: &str, payload: &mut [u8]) -> Result<(), KeyAccessError> {
    if target.is_empty() || target.len() > 256 || target.chars().any(char::is_control)
        || label.trim().is_empty() || label.len() > 128 || label.chars().any(char::is_control)
        || !valid_payload(payload)
    {
        return Err(KeyAccessError::Invalid);
    }
    let mut target: Vec<u16> = target.encode_utf16().chain([0]).collect();
    let mut label: Vec<u16> = label.encode_utf16().chain([0]).collect();
    // CredWriteW에는 create-only 플래그가 없다. 앱의 쓰기 경쟁은 이 잠금으로 막는다.
    let _guard = WRITES.lock().map_err(|_| KeyAccessError::Unavailable)?;
    match read_raw(&target) {
        Ok(_) => return Err(KeyAccessError::AlreadyExists),
        Err(KeyAccessError::Missing) => {}
        Err(error) => return Err(error),
    }
    let value = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: target.as_mut_ptr(),
        Comment: label.as_mut_ptr(),
        CredentialBlobSize: payload.len() as u32,
        CredentialBlob: payload.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    if unsafe { CredWriteW(&value, 0) } == 0 {
        return Err(KeyAccessError::Unavailable);
    }
    let saved = read_raw(&target).map_err(|_| KeyAccessError::OutcomeUnknown)?;
    if unsafe { (*saved.0).Persist } == CRED_PERSIST_LOCAL_MACHINE
        && saved.blob().is_ok_and(|blob| blob == payload)
    {
        Ok(())
    } else {
        Err(KeyAccessError::OutcomeUnknown)
    }
}

fn valid_payload(payload: &[u8]) -> bool {
    payload.len() <= CRED_MAX_CREDENTIAL_BLOB_SIZE as usize
        && std::str::from_utf8(payload).ok().and_then(|text| text.split_once('\n'))
            .is_some_and(|(id, secret)| super::valid_payload_fields(id, secret)
                && [id, secret].into_iter().all(|field| field.encode_utf16().count() <= MAX_FIELD_UNITS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_payload_preserves_utf8_and_rejects_invalid_fields() {
        assert!(valid_payload("한글\nsecret".as_bytes()));
        assert!(!valid_payload(b"id\nsecret\n"));
        assert!(!valid_payload(b"\nsecret"));
        assert!(!valid_payload(b"id\n\xff"));
        assert!(!valid_payload(format!("id\n{}", "a".repeat(MAX_FIELD_UNITS + 1)).as_bytes()));
    }
}
