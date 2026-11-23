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

fn key(dns: &str, endpoint: &str) -> String {
    format!("{dns}\n{endpoint}")
}

/// 손상되거나 읽을 수 없는 파일은 빈 고정 목록으로 취급한다. 조용히 신뢰하지 않는다.
pub(super) fn load(path: &Path, dns: &str, endpoint: &str) -> Option<String> {
    let file: File = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    file.pins.get(&key(dns, endpoint)).map(|pin| pin.fingerprint.clone())
}

pub(super) fn save(path: &Path, dns: &str, endpoint: &str, fingerprint: &str) -> Result<(), Error> {
    let mut file: File = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    file.version = 1;
    file.pins.insert(
        key(dns, endpoint),
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

pub(super) struct Trust<A> {
    pub path: PathBuf,
    pub dns: String,
    pub endpoint: String,
    pub approve: A,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> PathBuf {
        std::env::temp_dir().join(format!("zzabba-rdp-trust-{}", std::process::id()))
    }

    #[test]
    fn stores_and_reloads_one_fingerprint_per_target() {
        let path = path();
        let _ = fs::remove_file(&path);
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3389"), None);
        save(&path, "rdp.example", "127.0.0.1:3389", "AA:BB").unwrap();
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3389").as_deref(), Some("AA:BB"));
        assert_eq!(load(&path, "other.example", "127.0.0.1:3389"), None);
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn corrupted_store_starts_empty_instead_of_trusting() {
        let path = path();
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3389"), None);
        save(&path, "rdp.example", "127.0.0.1:3389", "CC:DD").unwrap();
        assert_eq!(load(&path, "rdp.example", "127.0.0.1:3389").as_deref(), Some("CC:DD"));
        fs::remove_file(&path).unwrap();
    }
}
