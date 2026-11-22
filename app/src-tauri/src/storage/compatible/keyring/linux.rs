use std::{
    ffi::{CString, c_char, c_int, c_uint, c_void},
    ptr::{null, null_mut},
    slice,
    sync::LazyLock,
};

use libloading::Library;

use super::{APPLICATION, KeyAccessError, ServiceKey, decode};

type Ptr = *mut c_void;
type Release = unsafe extern "C" fn(Ptr);

#[repr(C)]
struct List {
    data: Ptr,
    next: *mut List,
    prev: *mut List,
}

struct Owned(Ptr, Release);

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { (self.1)(self.0) }
    }
}

struct SearchResults(*mut List, unsafe extern "C" fn(*mut List, Release), Release);

impl Drop for SearchResults {
    fn drop(&mut self) {
        unsafe { (self.1)(self.0, self.2) }
    }
}

// GLib에 등록된 타입이 라이브러리의 코드 주소를 유지하므로 성공한 로드는 프로세스 수명 동안 유지한다.
static LIBSECRET: LazyLock<Option<Library>> =
    LazyLock::new(|| unsafe { Library::new("libsecret-1.so.0").ok() });

pub(super) fn read(target: &str) -> Result<ServiceKey, KeyAccessError> {
    let target = CString::new(target).map_err(|_| KeyAccessError::Invalid)?;
    let application = CString::new(APPLICATION).map_err(|_| KeyAccessError::Invalid)?;
    unsafe {
        let library = LIBSECRET.as_ref().ok_or(KeyAccessError::Unavailable)?;
        macro_rules! symbol {
            ($name:ident: $ty:ty) => {
                *library
                    .get::<$ty>(stringify!($name).as_bytes())
                    .map_err(|_| KeyAccessError::Unavailable)?
            };
        }
        let hash = symbol!(g_str_hash: unsafe extern "C" fn(*const c_void) -> c_uint);
        let equal =
            symbol!(g_str_equal: unsafe extern "C" fn(*const c_void, *const c_void) -> c_int);
        let table_new = symbol!(g_hash_table_new: unsafe extern "C" fn(
            unsafe extern "C" fn(*const c_void) -> c_uint,
            unsafe extern "C" fn(*const c_void, *const c_void) -> c_int,
        ) -> Ptr);
        let table_unref = symbol!(g_hash_table_unref: Release);
        let insert = symbol!(g_hash_table_insert: unsafe extern "C" fn(Ptr, *const c_char, *const c_char) -> c_int);
        let search = symbol!(secret_service_search_sync: unsafe extern "C" fn(
            Ptr, *const c_void, Ptr, c_uint, Ptr, *mut Ptr,
        ) -> *mut List);
        let free_list = symbol!(g_list_free_full: unsafe extern "C" fn(*mut List, Release));
        let unref = symbol!(g_object_unref: Release);
        let error_free = symbol!(g_error_free: Release);
        let service_get =
            symbol!(secret_service_get_sync: unsafe extern "C" fn(c_uint, Ptr, *mut Ptr) -> Ptr);
        let is_locked = symbol!(secret_item_get_locked: unsafe extern "C" fn(Ptr) -> c_int);
        let load = symbol!(secret_item_load_secret_sync: unsafe extern "C" fn(Ptr, Ptr, *mut Ptr) -> c_int);
        let get = symbol!(secret_item_get_secret: unsafe extern "C" fn(Ptr) -> Ptr);
        let bytes = symbol!(secret_value_get: unsafe extern "C" fn(Ptr, *mut usize) -> *const u8);
        let value_unref = symbol!(secret_value_unref: Release);

        let attributes = Owned(table_new(hash, equal), table_unref);
        insert(attributes.0, c"application".as_ptr(), application.as_ptr());
        insert(attributes.0, c"target".as_ptr(), target.as_ptr());
        // SECRET_SEARCH_ALL만 사용해서 중복 항목을 검출한다. 검색 중 잠금 해제나 비밀 로드는 없다.
        const SECRET_SEARCH_ALL: c_uint = 1 << 1;
        let mut error = null_mut();
        let service = service_get(0, null_mut(), &mut error);
        if !error.is_null() {
            error_free(error);
            return Err(KeyAccessError::Unavailable);
        }
        if service.is_null() {
            return Err(KeyAccessError::Unavailable);
        }
        let service = Owned(service, unref);
        let mut error = null_mut();
        let items = search(
            service.0,
            null(),
            attributes.0,
            SECRET_SEARCH_ALL,
            null_mut(),
            &mut error,
        );
        let items = SearchResults(items, free_list, unref);
        if !error.is_null() {
            error_free(error);
            return Err(KeyAccessError::Unavailable);
        }
        let item = items.0.as_ref().ok_or(KeyAccessError::Missing)?;
        if !item.next.is_null() {
            return Err(KeyAccessError::Invalid);
        }
        if is_locked(item.data) != 0 {
            return Err(KeyAccessError::Locked);
        }
        if load(item.data, null_mut(), null_mut()) == 0 {
            return Err(if is_locked(item.data) != 0 {
                KeyAccessError::Locked
            } else {
                KeyAccessError::Unavailable
            });
        }
        let value = get(item.data);
        if value.is_null() {
            return Err(KeyAccessError::Unavailable);
        }
        let value = Owned(value, value_unref);
        let mut len = 0;
        let payload = bytes(value.0, &mut len);
        if payload.is_null() {
            return Err(KeyAccessError::Invalid);
        }
        decode(slice::from_raw_parts(payload, len))
    }
}
