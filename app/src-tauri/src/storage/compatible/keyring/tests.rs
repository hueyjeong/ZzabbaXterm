use std::cell::Cell;

use super::super::{AddressStyle, CompatibleService, ServiceKeyAlias, ServiceKeyRef};
use super::*;

const ITEM: &str = "test-item-1";
const MARKERS: &[u8] = b"S02_TEST_ONLY_ACCESS_KEY\nS02_TEST_ONLY_SECRET";

fn service(id: &str, url: &str) -> CompatibleService {
    CompatibleService::new(id.into(), url, "us-east-1".into(), AddressStyle::Path).unwrap()
}

fn alias() -> ServiceKeyAlias {
    ServiceKeyAlias::new("시험 키".into()).unwrap()
}

#[test]
fn reference_lookup_is_bound_to_normalized_origin_service_and_item() {
    let original = service("test", "https://Store.Example.Test:443/");
    let equivalent = service("test", "https://store.example.test");
    let expected = target_name(original.scope(), ITEM).unwrap();
    assert_eq!(expected, target_name(equivalent.scope(), ITEM).unwrap());
    assert_ne!(
        expected,
        target_name(original.scope(), "test-item-2").unwrap()
    );

    let reference = ServiceKeyRef::new(&original, ITEM.into()).unwrap();
    for other in [
        service("another", "https://store.example.test"),
        service("test", "https://another.example.test"),
        service("test", "https://store.example.test:8443"),
    ] {
        assert_ne!(expected, target_name(other.scope(), ITEM).unwrap());
        assert_eq!(
            other
                .connect(&reference, alias(), |scope, id| {
                    read_with(
                        scope,
                        id,
                        |_, _| panic!("unexpected approval"),
                        |_| panic!("unexpected vault read"),
                    )
                })
                .err(),
            Some(KeyAccessError::WrongService)
        );
    }

    let approved = Cell::new(false);
    equivalent
        .connect(&reference, alias(), |scope, id| {
            read_with(
                scope,
                id,
                |scope, item| {
                    assert_eq!(scope, equivalent.scope());
                    assert_eq!(item, ITEM);
                    approved.set(true);
                    true
                },
                |target| {
                    assert!(approved.get());
                    assert_eq!(target, expected);
                    let key = decode(MARKERS)?;
                    assert!(key.0.access_key_id() == "S02_TEST_ONLY_ACCESS_KEY");
                    assert!(key.0.secret_access_key() == "S02_TEST_ONLY_SECRET");
                    Ok(key)
                },
            )
        })
        .unwrap();
}

#[test]
fn cancelled_and_invalid_items_skip_the_vault_and_vault_errors_stop_at_source() {
    let service = service("test", "https://store.example.test");
    assert_eq!(
        read_with(
            service.scope(),
            ITEM,
            |_, _| false,
            |_| { panic!("unexpected vault read") }
        )
        .err(),
        Some(KeyAccessError::Cancelled)
    );
    assert_eq!(
        read_approved(service.scope(), ITEM, |_, _| false).err(),
        Some(KeyAccessError::Cancelled)
    );
    assert_eq!(
        read_with(
            service.scope(),
            "not/an/item",
            |_, _| panic!("unexpected approval"),
            |_| panic!("unexpected vault read"),
        )
        .err(),
        Some(KeyAccessError::Invalid)
    );
    for error in [
        KeyAccessError::Missing,
        KeyAccessError::Locked,
        KeyAccessError::Unavailable,
    ] {
        let reference = ServiceKeyRef::new(&service, ITEM.into()).unwrap();
        assert_eq!(
            service
                .connect(&reference, alias(), |scope, id| {
                    read_with(scope, id, |_, _| true, |_| Err(error))
                })
                .err(),
            Some(error)
        );
    }
}

#[test]
fn malformed_vault_payload_is_rejected_without_echoing_its_contents() {
    for payload in [
        &b"\xff"[..],
        b"missing-separator",
        b"\nS02_TEST_ONLY_SECRET",
        b"S02_TEST_ONLY_ACCESS_KEY\n",
        b"S02_TEST_ONLY_ACCESS_KEY\nS02_TEST_ONLY_SECRET\nextra",
        b"S02_TEST_ONLY_ACCESS_KEY\nS02_TEST_ONLY_SECRET\0",
    ] {
        assert_eq!(decode(payload).err(), Some(KeyAccessError::Invalid));
    }
}
