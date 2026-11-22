use std::path::{Path, PathBuf};

pub(crate) mod ipc;
pub(crate) mod probe;
pub(crate) mod profile;
mod store;

#[cfg(test)]
pub(crate) mod tests;

pub(crate) fn saved_local(directory: &Path, id: &str) -> Result<(profile::Shell, PathBuf), String> {
    let file = store::Store::new(directory).load().map_err(|error| format!("프로필 읽기: {error:?}"))?;
    let target = file.targets.into_iter().map(|entry| entry.profile)
        .find(|target| target.id.as_str() == id)
        .ok_or("저장된 연결 대상을 찾을 수 없습니다")?;
    match target.method {
        profile::Method::Local { local } => Ok((local.shell, local.start_directory)),
        profile::Method::Ssm { .. } => Err("SSM 터미널 열기를 사용할 수 없습니다".into()),
    }
}
