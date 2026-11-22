use std::{
    fs::File,
    io::{BufWriter, Read, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use super::{
    AddressStyle, CompatibleS3, CompatibleService, ConfigError, KeyAccessError, ServiceKey,
    ServiceKeyAlias, ServiceKeyRef, ServiceKeyScope, keyring,
};

const VERSION: u32 = 1;
const MAX_FILE_BYTES: usize = 8192;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SettingsInput {
    pub service_id: String,
    pub alias: String,
    pub endpoint: String,
    pub region: String,
    pub addressing: AddressStyle,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SettingsView {
    pub service_id: String,
    pub alias: String,
    pub origin: String,
    pub region: String,
    pub addressing: AddressStyle,
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum ConnectIntent {
    Cancel,
    Connect {
        service_id: String,
        endpoint: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SettingsError {
    InvalidEndpoint,
    InvalidRegion,
    InvalidServiceId,
    InvalidItemId,
    InvalidAlias,
    InvalidFile,
    UnsupportedVersion,
    NotFound,
    Io,
    WrongService,
    Cancelled,
    KeyMissing,
    KeyLocked,
    KeyUnavailable,
    KeyInvalid,
}

impl From<ConfigError> for SettingsError {
    fn from(error: ConfigError) -> Self {
        match error {
            ConfigError::Endpoint => Self::InvalidEndpoint,
            ConfigError::Region => Self::InvalidRegion,
            ConfigError::ServiceId => Self::InvalidServiceId,
            ConfigError::KeyId => Self::InvalidItemId,
            ConfigError::KeyAlias => Self::InvalidAlias,
        }
    }
}

impl From<KeyAccessError> for SettingsError {
    fn from(error: KeyAccessError) -> Self {
        match error {
            KeyAccessError::WrongService => Self::WrongService,
            KeyAccessError::Cancelled => Self::Cancelled,
            KeyAccessError::Missing => Self::KeyMissing,
            KeyAccessError::Locked => Self::KeyLocked,
            KeyAccessError::Unavailable => Self::KeyUnavailable,
            KeyAccessError::Invalid => Self::KeyInvalid,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileV1 {
    version: u32,
    service: SettingsView,
    key: BoundItem,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BoundItem {
    service_id: String,
    origin: String,
    item_id: String,
}

pub(crate) struct Settings {
    service: CompatibleService,
    alias: String,
    addressing: AddressStyle,
    key: ServiceKeyRef,
}

impl Settings {
    /// 선택 경계에서 만든 원래 참조를 받는다. 변경된 주소에 기존 항목을 다시 묶을 수 없다.
    #[cfg(test)]
    pub(crate) fn new(input: SettingsInput, key: ServiceKeyRef) -> Result<Self, SettingsError> {
        let service = CompatibleService::new(
            input.service_id,
            &input.endpoint,
            input.region,
            input.addressing,
        )?;
        Self::from_validated(service, input.alias, input.addressing, key)
    }

    pub(crate) fn select(input: SettingsInput, item_id: String) -> Result<Self, SettingsError> {
        let service = CompatibleService::new(input.service_id, &input.endpoint, input.region, input.addressing)?;
        let key = ServiceKeyRef::new(&service, item_id)?;
        Self::from_validated(service, input.alias, input.addressing, key)
    }

    pub(crate) fn item_id(&self) -> &str {
        self.key.item_id()
    }

    fn from_validated(
        service: CompatibleService,
        alias: String,
        addressing: AddressStyle,
        key: ServiceKeyRef,
    ) -> Result<Self, SettingsError> {
        let trimmed = alias.trim();
        if trimmed.is_empty() || trimmed.len() > 128 || trimmed.chars().any(char::is_control) {
            return Err(SettingsError::InvalidAlias);
        }
        let alias = if trimmed.len() == alias.len() {
            alias
        } else {
            trimmed.to_owned()
        };
        if service.scope() != key.scope() {
            return Err(SettingsError::WrongService);
        }
        Ok(Self {
            service,
            alias,
            addressing,
            key,
        })
    }

    pub(crate) fn view(&self) -> SettingsView {
        SettingsView {
            service_id: self.service.scope().service_id().into(),
            alias: self.alias.clone(),
            origin: self.service.scope().origin().into(),
            region: self.service.region().into(),
            addressing: self.addressing,
        }
    }

    /// 경로는 설정을 관리하는 Rust 호출자가 지정한다. 웹뷰 입력으로 받지 않는다.
    pub(crate) fn save(&self, path: &Path) -> Result<(), SettingsError> {
        let file = FileV1 {
            version: VERSION,
            service: self.view(),
            key: BoundItem {
                service_id: self.key.scope().service_id().into(),
                origin: self.key.scope().origin().into(),
                item_id: self.key.item_id().into(),
            },
        };
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut temp = NamedTempFile::new_in(parent).map_err(|_| SettingsError::Io)?;
        let mut writer = BufWriter::new(&mut temp);
        serde_json::to_writer(&mut writer, &file).map_err(|_| SettingsError::Io)?;
        writer.flush().map_err(|_| SettingsError::Io)?;
        drop(writer);
        temp.as_file().sync_all().map_err(|_| SettingsError::Io)?;
        temp.persist(path).map_err(|_| SettingsError::Io)?;
        Ok(())
    }

    pub(crate) fn load(path: &Path) -> Result<Self, SettingsError> {
        #[derive(Deserialize)]
        struct Version {
            version: u32,
        }

        let file = File::open(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SettingsError::NotFound
            } else {
                SettingsError::Io
            }
        })?;
        let mut bytes = Vec::new();
        file.take((MAX_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| SettingsError::Io)?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(SettingsError::InvalidFile);
        }
        let version: Version =
            serde_json::from_slice(&bytes).map_err(|_| SettingsError::InvalidFile)?;
        if version.version != VERSION {
            return Err(SettingsError::UnsupportedVersion);
        }
        let FileV1 { service, key, .. } =
            serde_json::from_slice(&bytes).map_err(|_| SettingsError::InvalidFile)?;
        let definition = CompatibleService::new(
            service.service_id,
            &service.origin,
            service.region,
            service.addressing,
        )?;
        if definition.scope().origin() != service.origin {
            return Err(SettingsError::InvalidFile);
        }
        if key.service_id != definition.scope().service_id() || key.origin != service.origin {
            return Err(SettingsError::WrongService);
        }
        let key = ServiceKeyRef::new(&definition, key.item_id)?;
        Self::from_validated(definition, service.alias, service.addressing, key)
    }

    pub(crate) fn connect(
        &self,
        intent: ConnectIntent,
        approve: impl FnOnce(&ServiceKeyScope, &str) -> bool,
    ) -> Result<CompatibleS3, SettingsError> {
        self.connect_with(intent, |scope, item| {
            keyring::read_approved(scope, item, approve)
        })
    }

    fn connect_with(
        &self,
        intent: ConnectIntent,
        read: impl FnOnce(&ServiceKeyScope, &str) -> Result<ServiceKey, KeyAccessError>,
    ) -> Result<CompatibleS3, SettingsError> {
        let ConnectIntent::Connect {
            service_id,
            endpoint,
        } = intent
        else {
            return Err(SettingsError::Cancelled);
        };
        let requested = CompatibleService::new(
            service_id,
            &endpoint,
            self.service.region().into(),
            self.addressing,
        )?;
        if requested.scope() != self.service.scope() {
            return Err(SettingsError::WrongService);
        }
        let alias = ServiceKeyAlias::new(self.alias.clone())?;
        Ok(self.service.connect(&self.key, alias, read)?)
    }
}

#[cfg(test)]
#[path = "settings/tests.rs"]
mod tests;
