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
    match target.default_method {
        profile::MethodId::Local => Ok((target.local.shell, target.local.start_directory)),
    }
}

pub(crate) fn with_ai_targets<T>(
    directory: &Path,
    read: impl FnOnce(Vec<crate::ai::SelectedTarget>) -> T,
) -> Result<T, String> {
    ipc::prepare_config_parent(directory)?;
    store::Store::new(directory)
        .with_loaded(|file| {
            let targets = file.targets.into_iter().map(|entry| crate::ai::SelectedTarget {
                profile_id: entry.profile.id.as_str().to_owned(),
                display_name: entry.profile.display_name,
                region: String::new(),
                node_id: "내 PC".into(),
                generation: entry.revision,
            }).collect();
            read(targets)
        })
        .map_err(|_| "저장된 조회 대상을 읽을 수 없습니다".into())
}
