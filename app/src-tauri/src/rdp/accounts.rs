//! 연결 대상별로 마지막에 로그인한 계정 이름(`도메인\계정`)만 기억한다. 비밀번호는 저장하지 않는다.

use std::{collections::BTreeMap, fs, io::Write as _, path::Path, sync::Mutex};

use serde::{Deserialize, Serialize};

const MAX_LOGIN: usize = 256;
/// 여러 탭이 거의 동시에 연결을 마쳐도 읽고 고쳐 쓰기가 겹치지 않게 한다.
static WRITE: Mutex<()> = Mutex::new(());

#[derive(Default, Deserialize, Serialize)]
struct File {
    version: u32,
    logins: BTreeMap<String, String>,
}

/// 인증서 고정과 같은 대상 구분이다. 전달 경로는 루프백 endpoint가 같아도 경로 대상 ID로 나눈다.
fn key(dns: &str, endpoint: &str, route: Option<&str>) -> String {
    match route {
        Some(route) => format!("{dns}\n{endpoint}\n{route}"),
        None => format!("{dns}\n{endpoint}"),
    }
}

fn valid(login: &str) -> bool {
    !login.is_empty() && login.chars().count() <= MAX_LOGIN && login.trim() == login && !login.chars().any(char::is_control)
}

/// 읽을 수 없거나 형식이 틀린 파일은 기억한 계정이 없는 것으로 본다.
pub(super) fn load(path: &Path, dns: &str, endpoint: &str, route: Option<&str>) -> Option<String> {
    let file: File = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    file.logins.get(&key(dns, endpoint, route)).filter(|login| valid(login)).cloned()
}

/// 세션이 열린 계정 이름을 원자적으로 바꿔 쓴다. 실패해도 연결은 계속한다.
pub(super) fn save(path: &Path, dns: &str, endpoint: &str, route: Option<&str>, login: &str) -> std::io::Result<()> {
    if !valid(login) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let _write = WRITE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut file: File = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let key = key(dns, endpoint, route);
    if file.logins.get(&key).is_some_and(|saved| saved == login) {
        return Ok(());
    }
    file.version = 1;
    file.logins.insert(key, login.to_owned());
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }
    let temporary = path.with_extension("tmp");
    let mut out = fs::File::create(&temporary)?;
    out.write_all(&serde_json::to_vec(&file)?)?;
    out.sync_all().ok();
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::tests::TestDir;

    #[test]
    fn remembers_one_login_per_target_without_password() {
        let dir = TestDir::new();
        let path = dir.path().join("rdp-accounts.json");
        assert_eq!(load(&path, "rdp.example", "rdp.example:3389", None), None);
        save(&path, "rdp.example", "rdp.example:3389", None, "CORP\\demo").unwrap();
        save(&path, "rdp.example", "127.0.0.1:3390", Some("ssh-a"), "tunnel").unwrap();
        assert_eq!(load(&path, "rdp.example", "rdp.example:3389", None).as_deref(), Some("CORP\\demo"));
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3390", Some("ssh-a")).as_deref(), Some("tunnel"));
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3390", Some("ssh-b")), None);
        save(&path, "rdp.example", "rdp.example:3389", None, "other").unwrap();
        assert_eq!(load(&path, "rdp.example", "rdp.example:3389", None).as_deref(), Some("other"));
        let stored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored, serde_json::json!({ "version": 1, "logins": {
            "rdp.example\nrdp.example:3389": "other",
            "rdp.example\n127.0.0.1:3390\nssh-a": "tunnel",
        }}));
    }

    #[test]
    fn rejects_invalid_logins_and_ignores_a_corrupted_store() {
        let dir = TestDir::new();
        let path = dir.path().join("rdp-accounts.json");
        for login in ["", " demo", "demo\n", &"a".repeat(MAX_LOGIN + 1)] {
            assert!(save(&path, "rdp.example", "rdp.example:3389", None, login).is_err());
        }
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(load(&path, "rdp.example", "rdp.example:3389", None), None);
        save(&path, "rdp.example", "rdp.example:3389", None, "demo").unwrap();
        assert_eq!(load(&path, "rdp.example", "rdp.example:3389", None).as_deref(), Some("demo"));
    }
}
