use std::path::Path;

pub(crate) mod ipc;
pub(crate) mod probe;
pub(crate) mod profile;
mod store;

#[cfg(test)]
pub(crate) mod tests;

pub(crate) fn register(directory: &Path, draft: profile::ValidatedDraft) -> Result<(String, String), crate::ai::cli::Error> {
    use crate::ai::cli::Error;

    ipc::prepare_config_parent(directory).map_err(|_| Error::StoreUnavailable)?;
    let id = draft.target_id().to_owned();
    match store::Store::new(directory).register(draft).map_err(|_| Error::StoreUnavailable)? {
        store::SaveStatus::Saved => Ok((id, profile::Revision::MIN.to_string())),
        store::SaveStatus::GroupConflict => Err(Error::GroupConflict),
        store::SaveStatus::TargetConflict => Err(Error::TargetConflict),
    }
}
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
            let targets = file.targets.into_iter().filter_map(|entry| {
                let (ssm_profile, region, node_id) = match entry.profile.method() {
                    profile::MethodRef::Local { .. } => (None, String::new(), "내 PC".into()),
                    profile::MethodRef::Ssm { profile, region, instance_id } => (Some(profile.into()), region.into(), instance_id.into()),
                    profile::MethodRef::Ssh { .. } => return None,
                };
                Some(crate::ai::SelectedTarget {
                    profile_id: entry.profile.id.as_str().to_owned(),
                    display_name: entry.profile.display_name,
                    ssm_profile,
                    region,
                    node_id,
                    generation: entry.revision,
                })
            }).collect();
            read(targets)
        })
        .map_err(|_| "저장된 조회 대상을 읽을 수 없습니다".into())
}

pub(crate) fn ai_aliases(directory: &Path) -> Result<Vec<crate::ai::cli::Alias>, String> {
    ipc::prepare_config_parent(directory)?;
    let file = store::Store::new(directory).load()
        .map_err(|_| "저장된 조회 대상을 읽을 수 없습니다")?;
    Ok(file.targets.into_iter().filter_map(|entry| {
        let method = match entry.profile.method() {
            profile::MethodRef::Local { .. } => "local",
            profile::MethodRef::Ssm { .. } => "ssm",
            profile::MethodRef::Ssh { .. } => return None,
        };
        Some(crate::ai::cli::Alias {
            id: entry.profile.id.as_str().into(),
            generation: entry.revision.to_string(),
            method: method.into(),
            display_name: entry.profile.display_name,
        })
    }).collect())
}
