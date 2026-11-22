use std::path::Path;

pub(crate) mod ipc;
pub(crate) mod probe;
pub(crate) mod profile;
mod store;

#[cfg(test)]
pub(crate) mod tests;

pub(crate) fn with_saved_method<T>(directory: &Path, id: &str, use_method: impl FnOnce(profile::MethodRef<'_>) -> Result<T, String>) -> Result<T, String> {
    let file = store::Store::new(directory).load().map_err(|error| format!("프로필 읽기: {error:?}"))?;
    let target = file.targets.into_iter().map(|entry| entry.profile)
        .find(|target| target.id.as_str() == id)
        .ok_or("저장된 연결 대상을 찾을 수 없습니다")?;
    use_method(target.method())
}

pub(crate) fn with_ai_targets<T>(
    directory: &Path,
    read: impl FnOnce(Vec<crate::ai::SelectedTarget>) -> T,
) -> Result<T, String> {
    ipc::prepare_config_parent(directory)?;
    store::Store::new(directory)
        .with_loaded(|file| {
            let targets = file.targets.into_iter().map(|entry| {
                let (region, node_id) = match entry.profile.method() {
                    profile::MethodRef::Local { .. } => (String::new(), "내 PC".into()),
                    profile::MethodRef::Ssm { region, instance_id, .. } => (region.into(), instance_id.into()),
                };
                crate::ai::SelectedTarget {
                    profile_id: entry.profile.id.as_str().to_owned(),
                    display_name: entry.profile.display_name,
                    region,
                    node_id,
                    generation: entry.revision,
                }
            }).collect();
            read(targets)
        })
        .map_err(|_| "저장된 조회 대상을 읽을 수 없습니다".into())
}

pub(crate) fn ai_aliases(directory: &Path) -> Result<Vec<crate::ai::cli::Alias>, String> {
    ipc::prepare_config_parent(directory)?;
    let file = store::Store::new(directory).load()
        .map_err(|_| "저장된 조회 대상을 읽을 수 없습니다")?;
    Ok(file.targets.into_iter().map(|entry| crate::ai::cli::Alias {
        id: entry.profile.id.as_str().into(),
        generation: entry.revision.to_string(),
        method: match entry.profile.method() {
            profile::MethodRef::Local { .. } => "local",
            profile::MethodRef::Ssm { .. } => "ssm",
        }.into(),
        display_name: entry.profile.display_name,
    }).collect())
}
