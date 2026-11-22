use serde_json::{Value, json};

use super::*;

const ENDPOINT: &str = "https://Store.Example.Test:443/";
const ORIGIN: &str = "https://store.example.test";
const ITEM: &str = "opaque-item-01";

fn input(endpoint: &str, alias: &str) -> SettingsInput {
    SettingsInput {
        service_id: "sample".into(),
        alias: alias.into(),
        endpoint: endpoint.into(),
        region: "us-east-1".into(),
        addressing: AddressStyle::VirtualHosted,
    }
}

fn reference() -> ServiceKeyRef {
    let service = CompatibleService::new(
        "sample".into(),
        ORIGIN,
        "us-east-1".into(),
        AddressStyle::VirtualHosted,
    )
    .unwrap();
    ServiceKeyRef::new(&service, ITEM.into()).unwrap()
}

#[test]
fn settings_replace_atomically_and_expose_only_the_public_view() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compatible.json");
    assert_eq!(Settings::load(&path).err(), Some(SettingsError::NotFound));
    Settings::new(input(ENDPOINT, "이전"), reference())
        .unwrap()
        .stage_save(path.clone())
        .unwrap()
        .publish()
        .unwrap();
    let settings = Settings::new(input(ENDPOINT, " 새 별칭 "), reference()).unwrap();
    settings.stage_save(path.clone()).unwrap().publish().unwrap();

    let view = json!({
        "serviceId": "sample", "alias": "새 별칭", "origin": ORIGIN,
        "region": "us-east-1", "addressing": "virtual_hosted"
    });
    let file: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        file,
        json!({
            "version": 1, "service": view,
            "key": {"serviceId": "sample", "origin": ORIGIN, "itemId": ITEM}
        })
    );
    let loaded = Settings::load(&path).unwrap();
    assert_eq!(serde_json::to_value(loaded.view()).unwrap(), view);
    assert_eq!(loaded.key.item_id(), ITEM);

    let blocked = dir.path().join("existing-directory");
    std::fs::create_dir(&blocked).unwrap();
    assert!(settings.stage_save(blocked.clone()).unwrap().publish().is_err());
    assert!(blocked.is_dir());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn edited_endpoint_and_cancel_stop_before_approval_or_key_read() {
    let other = "https://other.example.test";
    assert_eq!(
        Settings::new(input(other, "test"), reference()).err(),
        Some(SettingsError::WrongService)
    );
    let settings = Settings::new(input(ENDPOINT, "test"), reference()).unwrap();
    for (wire, error) in [
        (json!({"kind": "cancel"}), SettingsError::Cancelled),
        (
            json!({"kind": "connect", "serviceId": "sample", "endpoint": other}),
            SettingsError::WrongService,
        ),
        (
            json!({"kind": "connect", "serviceId": "other", "endpoint": ORIGIN}),
            SettingsError::WrongService,
        ),
        (
            json!({"kind": "connect", "serviceId": "sample", "endpoint": "http://store.example.test"}),
            SettingsError::InvalidEndpoint,
        ),
    ] {
        let intent = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(
            settings
                .connect(intent, |_, _| panic!("unexpected approval"))
                .err(),
            Some(error)
        );
        let intent = serde_json::from_value(wire).unwrap();
        assert_eq!(
            settings
                .connect_with(intent, |_, _| panic!("unexpected key read"))
                .err(),
            Some(error)
        );
    }
    let matching = || ConnectIntent::Connect {
        service_id: "sample".into(),
        endpoint: ENDPOINT.into(),
    };
    assert_eq!(
        settings
            .connect(matching(), |scope, item| {
                assert_eq!((scope.origin(), item), (ORIGIN, ITEM));
                false
            })
            .err(),
        Some(SettingsError::Cancelled)
    );
    assert_eq!(
        settings
            .connect_with(matching(), |scope, item| {
                assert_eq!((scope.origin(), item), (ORIGIN, ITEM));
                Err(KeyAccessError::Locked)
            })
            .err(),
        Some(SettingsError::KeyLocked)
    );
}

#[test]
fn load_rejects_unknown_versions_and_inconsistent_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compatible.json");
    let settings = Settings::new(input(ENDPOINT, "test"), reference()).unwrap();
    settings.stage_save(path.clone()).unwrap().publish().unwrap();
    let original: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for (pointer, replacement, error) in [
        ("/version", json!(2), SettingsError::UnsupportedVersion),
        (
            "/service/origin",
            json!("https://other.example.test"),
            SettingsError::WrongService,
        ),
        (
            "/key/serviceId",
            json!("other"),
            SettingsError::WrongService,
        ),
        (
            "/key/itemId",
            json!("not/an/item"),
            SettingsError::InvalidItemId,
        ),
    ] {
        let mut changed = original.clone();
        *changed.pointer_mut(pointer).unwrap() = replacement;
        std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert_eq!(Settings::load(&path).err(), Some(error));
    }
    let mut changed = original;
    changed["key"]["unknown"] = json!("S02_TEST_ONLY_MARKER");
    std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert_eq!(
        Settings::load(&path).err(),
        Some(SettingsError::InvalidFile)
    );
}
