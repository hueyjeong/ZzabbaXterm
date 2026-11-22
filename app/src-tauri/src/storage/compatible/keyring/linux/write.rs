use std::{ffi::{CStr, CString, c_char, c_int, c_uint, c_void}, ptr::{null, null_mut}};

use gtk::{gio::{self, prelude::*}, glib::{GString, Variant, VariantTy, translate::*, variant::ToVariant}};
use zeroize::Zeroize;

use super::{APPLICATION, Attributes, KeyAccessError, LIBSECRET, List, Owned, Ptr, Release, SECRET_SEARCH_ALL, SearchResults};

pub(crate) fn write_new(target: &str, label: &str, payload: &mut [u8]) -> Result<(), KeyAccessError> {
    let result = create(target, label, payload);
    payload.zeroize();
    result
}

fn create(target: &str, label: &str, payload: &[u8]) -> Result<(), KeyAccessError> {
    let attributes = Attributes::new(target)?;
    let label = CString::new(label).map_err(|_| KeyAccessError::Invalid)?;
    let len = isize::try_from(payload.len()).map_err(|_| KeyAccessError::Invalid)?;
    unsafe { with_symbols!(library, {
        let service_type = symbol!(secret_service_get_type: unsafe extern "C" fn() -> usize);
        let open = symbol!(secret_service_open_sync: unsafe extern "C" fn(usize, *const c_char, c_uint, Ptr, *mut Ptr) -> Ptr);
        let ensure_session = symbol!(secret_service_ensure_session_sync: unsafe extern "C" fn(Ptr, Ptr, *mut Ptr) -> c_int);
        let algorithm = symbol!(secret_service_get_session_algorithms: unsafe extern "C" fn(Ptr) -> *const c_char);
        let collection_for_alias = symbol!(secret_collection_for_alias_sync: unsafe extern "C" fn(Ptr, *const c_char, c_uint, Ptr, *mut Ptr) -> Ptr);
        let is_locked = symbol!(secret_collection_get_locked: unsafe extern "C" fn(Ptr) -> c_int);
        let search = symbol!(secret_service_search_sync: unsafe extern "C" fn(Ptr, *const c_void, Ptr, c_uint, Ptr, *mut Ptr) -> *mut List);
        let unref = symbol!(g_object_unref: Release);
        let free_list = symbol!(g_list_free_full: unsafe extern "C" fn(*mut List, Release));
        let error_free = symbol!(g_error_free: Release);
        let value_new = symbol!(secret_value_new: unsafe extern "C" fn(*const u8, isize, *const c_char) -> Ptr);
        let value_unref = symbol!(secret_value_unref: Release);
        let encode = symbol!(secret_service_encode_dbus_secret: unsafe extern "C" fn(Ptr, Ptr) -> *mut gtk::glib::ffi::GVariant);

        let service = open(service_type(), null(), 0, null_mut(), null_mut());
        if service.is_null() {
            return Err(KeyAccessError::Unavailable);
        }
        let service = Owned(service, unref);
        let service_proxy: Borrowed<gio::DBusProxy> = from_glib_borrow(service.0.cast());
        let owner = service_proxy.name_owner().ok_or(KeyAccessError::Unavailable)?;
        if ensure_session(service.0, null_mut(), null_mut()) == 0 {
            return Err(KeyAccessError::Unavailable);
        }
        let negotiated = algorithm(service.0);
        if negotiated.is_null() || CStr::from_ptr(negotiated) != c"dh-ietf1024-sha256-aes128-cbc-pkcs7" {
            return Err(KeyAccessError::Unavailable);
        }
        let collection = collection_for_alias(service.0, c"default".as_ptr(), 0, null_mut(), null_mut());
        if collection.is_null() {
            return Err(KeyAccessError::Unavailable);
        }
        let collection = Owned(collection, unref);
        if is_locked(collection.0) != 0 {
            return Err(KeyAccessError::Locked);
        }

        let mut error = null_mut();
        let items = SearchResults(search(service.0, null(), attributes.table.0, SECRET_SEARCH_ALL, null_mut(), &mut error), free_list, unref);
        if !error.is_null() {
            error_free(error);
            return Err(KeyAccessError::Unavailable);
        }
        if let Some(item) = items.0.as_ref() {
            return Err(if item.next.is_null() { KeyAccessError::AlreadyExists } else { KeyAccessError::Invalid });
        }

        let collection_proxy: Borrowed<gio::DBusProxy> = from_glib_borrow(collection.0.cast());
        if service_proxy.name_owner().as_deref() != Some(owner.as_str())
            || collection_proxy.name_owner().as_deref() != Some(owner.as_str())
        {
            return Err(KeyAccessError::Unavailable);
        }
        let value = value_new(payload.as_ptr(), len, c"application/octet-stream".as_ptr());
        if value.is_null() {
            return Err(KeyAccessError::Unavailable);
        }
        let value = Owned(value, value_unref);
        let encoded = encode(service.0, value.0);
        if encoded.is_null() {
            return Err(KeyAccessError::Unavailable);
        }
        let encoded: Variant = from_glib_none(encoded);
        let properties = std::collections::BTreeMap::from([
            ("org.freedesktop.Secret.Item.Label", label.to_str().map_err(|_| KeyAccessError::Invalid)?.to_variant()),
            ("org.freedesktop.Secret.Item.Attributes", std::collections::BTreeMap::from([("application", APPLICATION), ("target", target)]).to_variant()),
        ]).to_variant();
        let args = Variant::tuple_from_iter([properties, encoded, false.to_variant()]);
        if is_locked(collection.0) != 0 {
            return Err(KeyAccessError::Locked);
        }
        // 프롬프트가 반환돼도 실행하지 않고, 암호화 세션을 맺은 서비스 소유자에게만 전송한다.
        // shortcut: 검색과 생성은 원자적이지 않다. 외부 등록과 경합해도 교체는 금지되며 중복은 읽기에서 거부된다.
        let reply = service_proxy.connection().call_sync(
            Some(owner.as_str()), collection_proxy.object_path().as_str(),
            "org.freedesktop.Secret.Collection", "CreateItem", Some(&args),
            Some(VariantTy::new("(oo)").expect("정적 D-Bus 응답 타입")), gio::DBusCallFlags::NO_AUTO_START,
            30_000, gio::Cancellable::NONE,
        ).map_err(|error| {
            let remote: Option<GString> = from_glib_full(gio::ffi::g_dbus_error_get_remote_error(error.to_glib_none().0));
            match remote.as_deref() {
                Some("org.freedesktop.Secret.Error.IsLocked") => KeyAccessError::Locked,
                Some("org.freedesktop.Secret.Error.NoSuchObject") => KeyAccessError::Unavailable,
                _ => KeyAccessError::OutcomeUnknown,
            }
        })?;
        classify_reply(reply.child_value(0).str(), reply.child_value(1).str())
    }) }
}

fn classify_reply(item: Option<&str>, prompt: Option<&str>) -> Result<(), KeyAccessError> {
    match (item, prompt) {
        (Some("/"), Some(path)) if path != "/" => Err(KeyAccessError::Locked),
        (Some(path), Some("/")) if path != "/" => Ok(()),
        _ => Err(KeyAccessError::OutcomeUnknown),
    }
}

#[test]
fn only_an_item_without_a_prompt_confirms_creation() {
    assert_eq!(classify_reply(Some("/item"), Some("/")), Ok(()));
    assert_eq!(classify_reply(Some("/"), Some("/prompt")), Err(KeyAccessError::Locked));
    assert_eq!(classify_reply(Some("/"), Some("/")), Err(KeyAccessError::OutcomeUnknown));
    assert_eq!(classify_reply(Some("/item"), Some("/prompt")), Err(KeyAccessError::OutcomeUnknown));
}
