use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::Error;

#[derive(Default, Deserialize, Serialize)]
struct File {
    version: u32,
    pins: BTreeMap<String, Pin>,
}

#[derive(Deserialize, Serialize)]
struct Pin {
    fingerprint: String,
    approved_at: u64,
}

/// 고정 키다. 전달 경로(SSH, SSM)는 endpoint가 같은 루프백 주소여도 `route`(경로 대상 ID)로 구분한다.
fn key(dns: &str, endpoint: &str, route: Option<&str>) -> String {
    match route {
        Some(route) => format!("{dns}\n{endpoint}\n{route}"),
        None => format!("{dns}\n{endpoint}"),
    }
}

/// 손상되거나 읽을 수 없는 파일은 빈 고정 목록으로 취급한다. 조용히 신뢰하지 않는다.
/// 새 키가 없으면 예전 형식의 키를 읽는다. 예전 키에는 경로 대상 ID가 없고, IPv6 endpoint를
/// 괄호 없이(`::1:3389`) 적었다.
pub(super) fn load(path: &Path, dns: &str, endpoint: &str, route: Option<&str>) -> Option<String> {
    let file: File = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let unbracketed = endpoint.strip_prefix('[').and_then(|rest| rest.split_once("]:"))
        .map(|(host, port)| format!("{host}:{port}"));
    [Some(key(dns, endpoint, route)), route.map(|_| key(dns, endpoint, None)), unbracketed.map(|old| key(dns, &old, None))]
        .into_iter().flatten()
        .find_map(|key| file.pins.get(&key))
        .map(|pin| pin.fingerprint.clone())
}

/// 새 형식의 키로만 저장한다. 예전 키는 다른 경로의 고정일 수 있어 그대로 둔다.
pub(super) fn save(path: &Path, dns: &str, endpoint: &str, route: Option<&str>, fingerprint: &str) -> Result<(), Error> {
    let mut file: File = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    file.version = 1;
    file.pins.insert(
        key(dns, endpoint, route),
        Pin {
            fingerprint: fingerprint.into(),
            approved_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0),
        },
    );
    let directory = path.parent().ok_or(Error::TrustStore)?;
    fs::create_dir_all(directory).map_err(|_| Error::TrustStore)?;
    let temporary = path.with_extension("tmp");
    let mut out = fs::File::create(&temporary).map_err(|_| Error::TrustStore)?;
    out.write_all(&serde_json::to_vec(&file).map_err(|_| Error::TrustStore)?)
        .map_err(|_| Error::TrustStore)?;
    out.sync_all().ok();
    fs::rename(&temporary, path).map_err(|_| Error::TrustStore)
}

pub(super) fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub(super) struct Request {
    pub text: String,
    pub changed: bool,
}

pub(super) struct Trust<A> {
    pub path: PathBuf,
    pub dns: String,
    pub endpoint: String,
    /// 전달 경로의 대상 ID다. 직접 연결은 `None`이다.
    pub route: Option<String>,
    pub approve: A,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::tests::TestDir;

    #[test]
    fn stores_and_reloads_one_fingerprint_per_target() {
        let dir = TestDir::new();
        let path = dir.path().join("rdp-trust.json");
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3389", None), None);
        save(&path, "rdp.example", "127.0.0.1:3389", None, "AA:BB").unwrap();
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3389", None).as_deref(), Some("AA:BB"));
        assert_eq!(load(&path, "other.example", "127.0.0.1:3389", None), None);
    }

    #[test]
    fn corrupted_store_starts_empty_instead_of_trusting() {
        let dir = TestDir::new();
        let path = dir.path().join("rdp-trust.json");
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3389", None), None);
        save(&path, "rdp.example", "127.0.0.1:3389", None, "CC:DD").unwrap();
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3389", None).as_deref(), Some("CC:DD"));
    }

    #[test]
    fn forwarded_routes_on_the_same_loopback_endpoint_keep_separate_pins() {
        let dir = TestDir::new();
        let path = dir.path().join("rdp-trust.json");
        save(&path, "rdp.example", "127.0.0.1:3390", Some("ssh-a"), "AA:AA").unwrap();
        save(&path, "rdp.example", "127.0.0.1:3390", Some("ssh-b"), "BB:BB").unwrap();
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3390", Some("ssh-a")).as_deref(), Some("AA:AA"));
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3390", Some("ssh-b")).as_deref(), Some("BB:BB"));
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3390", Some("ssh-c")), None);
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3390", None), None);
    }

    #[test]
    fn pins_saved_under_the_previous_key_format_still_load() {
        let dir = TestDir::new();
        let path = dir.path().join("rdp-trust.json");
        // 예전 키: 경로 대상 ID 없이 `{dns}\n{host}:{port}`, IPv6 루프백은 괄호 없이 적었다.
        fs::write(&path, serde_json::to_vec(&serde_json::json!({ "version": 1, "pins": {
            "rdp.example\n127.0.0.1:3390": { "fingerprint": "V4:OLD", "approved_at": 1 },
            "rdp.example\n::1:3391": { "fingerprint": "V6:OLD", "approved_at": 1 },
        }})).unwrap()).unwrap();
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3390", Some("ssh-a")).as_deref(), Some("V4:OLD"));
        assert_eq!(load(&path, "rdp.example", "[::1]:3391", Some("ssh-a")).as_deref(), Some("V6:OLD"));
        // 새 고정은 새 키로 저장하고, 같은 경로는 새 키를 먼저 읽는다. 예전 키는 남는다.
        save(&path, "rdp.example", "[::1]:3391", Some("ssh-a"), "V6:NEW").unwrap();
        assert_eq!(load(&path, "rdp.example", "[::1]:3391", Some("ssh-a")).as_deref(), Some("V6:NEW"));
        assert_eq!(load(&path, "rdp.example", "[::1]:3391", Some("ssh-b")).as_deref(), Some("V6:OLD"));
        let file: File = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(file.pins.contains_key("rdp.example\n[::1]:3391\nssh-a"));
        assert!(file.pins.contains_key("rdp.example\n::1:3391"));
    }
}
