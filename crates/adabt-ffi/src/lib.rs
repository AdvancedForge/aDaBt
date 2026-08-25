//! The C ABI: a narrow, stable door into the engine.
//!
//! This is deliberately *not* the whole database. A C consumer gets exactly
//! what an embedding needs — open, close, one collection, integer fields in
//! and out, a count — through functions whose signatures contain nothing
//! that is not ABI-stable: pointers to NUL-terminated UTF-8, `u64`/`i64`
//! integers, and `i32` status codes. Every Rust type that owns memory stays
//! behind the boundary; nothing crosses it by value except what C can copy.
//!
//! Status codes are the contract's error language, mirrored in
//! `include/adabt.h`; the tests pin every pair.
//!
//! # Contract
//!
//! - Pointers returned by `adabt_db_open` belong to the caller until passed
//!   to `adabt_db_close`. Passing any other pointer is undefined.
//! - Strings passed in must be valid NUL-terminated UTF-8 and stay alive
//!   for the duration of the call.
//! - One thread at a time per handle. The engine serializes on its own
//!   lock, but two threads sharing one handle still race each other's
//!   transactions, as they would against any single connection.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_engine::database::Database;
use std::ffi::{c_char, c_int, c_longlong, c_ulonglong, CStr};

/// Status codes shared with `include/adabt.h`. Keep the two in lockstep;
/// the ABI tests pin every pair.
pub mod status {
    pub const OK: i32 = 0;
    pub const NO_SUCH_COLLECTION: i32 = 1;
    pub const COLLECTION_EXISTS: i32 = 2;
    pub const INVALID_ARGUMENT: i32 = 3;
    pub const NOT_FOUND: i32 = 4;
    pub const INTERNAL: i32 = 5;
}

fn code_of(e: &adabt_core::error::Error) -> i32 {
    use adabt_core::error::Error::*;
    match e {
        NoSuchCollection(_) => status::NO_SUCH_COLLECTION,
        CollectionExists(_) => status::COLLECTION_EXISTS,
        _ => status::INTERNAL,
    }
}

/// Open (or create) a database directory. Returns NULL on failure.
///
/// # Safety
///
/// `path` must be NULL or a valid NUL-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn adabt_db_open(path: *const c_char) -> *mut Database {
    let Some(path) = (unsafe { path.as_ref() }) else {
        return std::ptr::null_mut();
    };
    let Ok(path) = (unsafe { CStr::from_ptr(path) }).to_str() else {
        return std::ptr::null_mut();
    };
    match Database::open(std::path::Path::new(path), Policy::manual(0)) {
        Ok(db) => Box::into_raw(Box::new(db)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Close a database opened by `adabt_db_open`. NULL is accepted and
/// ignored, matching `free`.
///
/// # Safety
///
/// `db` must be NULL or a pointer returned by `adabt_db_open` that has not
/// already been closed.
#[no_mangle]
pub unsafe extern "C" fn adabt_db_close(db: *mut Database) {
    if !db.is_null() {
        drop(unsafe { Box::from_raw(db) });
    }
}

/// Create a dynamic-schema collection. Refuses duplicates with
/// `ADABT_COLLECTION_EXISTS`.
///
/// # Safety
///
/// `db` must be a live handle from `adabt_db_open`; `name` must be NULL or
/// a valid NUL-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn adabt_db_create_collection(
    db: *mut Database,
    name: *const c_char,
) -> c_int {
    let Some(db) = (unsafe { db.as_mut() }) else {
        return status::INVALID_ARGUMENT;
    };
    let Some(name) = str_of(name) else {
        return status::INVALID_ARGUMENT;
    };
    match db.create_collection(&name, Schema::dynamic()) {
        Ok(()) => status::OK,
        Err(e) => code_of(&e),
    }
}

/// Set one integer field on the record with `id`, creating or replacing the
/// whole record with that single field. Returns a status code.
///
/// # Safety
///
/// `db` must be a live handle; `collection` and `field` must be NULL or
/// valid NUL-terminated UTF-8 C strings.
#[no_mangle]
pub unsafe extern "C" fn adabt_db_put_i64(
    db: *mut Database,
    collection: *const c_char,
    id: c_ulonglong,
    field: *const c_char,
    value: i64,
) -> c_int {
    let Some(db) = (unsafe { db.as_mut() }) else {
        return status::INVALID_ARGUMENT;
    };
    let (Some(collection), Some(field)) = (str_of(collection), str_of(field)) else {
        return status::INVALID_ARGUMENT;
    };
    let rec = Record::new().with(field, value);
    match db.insert(&collection, RecordId(id), rec) {
        Ok(()) => status::OK,
        Err(e) => code_of(&e),
    }
}

/// Read one integer field. `*out` is written only when the return is
/// `ADABT_OK`; a missing record or field yields `ADABT_NOT_FOUND`.
///
/// # Safety
///
/// `db` must be a live handle; `collection` and `field` must be NULL or
/// valid NUL-terminated UTF-8 C strings; `out`, if non-NULL, must be
/// writable for one `i64`.
#[no_mangle]
pub unsafe extern "C" fn adabt_db_get_i64(
    db: *mut Database,
    collection: *const c_char,
    id: c_ulonglong,
    field: *const c_char,
    out: *mut i64,
) -> c_int {
    let Some(db) = (unsafe { db.as_mut() }) else {
        return status::INVALID_ARGUMENT;
    };
    if out.is_null() {
        return status::INVALID_ARGUMENT;
    }
    let (Some(collection), Some(field)) = (str_of(collection), str_of(field)) else {
        return status::INVALID_ARGUMENT;
    };
    match db.get(&collection, RecordId(id)) {
        Ok(Some(rec)) => match rec.get(&field) {
            Some(adabt_core::value::Value::I64(v)) => {
                unsafe { *out = *v };
                status::OK
            }
            _ => status::NOT_FOUND,
        },
        Ok(None) => status::NOT_FOUND,
        Err(e) => code_of(&e),
    }
}

/// Live record count in a collection, or -1 with a status in `out_status`
/// when the collection does not exist.
///
/// # Safety
///
/// `db` must be a live handle; `collection` must be NULL or a valid
/// NUL-terminated UTF-8 C string; `out_status`, if non-NULL, must be
/// writable for one `c_int`.
#[no_mangle]
pub unsafe extern "C" fn adabt_db_count(
    db: *mut Database,
    collection: *const c_char,
    out_status: *mut c_int,
) -> c_longlong {
    let fail = |code: c_int| -> c_longlong {
        unsafe {
            if let Some(s) = out_status.as_mut() {
                *s = code;
            }
        }
        -1
    };
    let Some(db) = (unsafe { db.as_mut() }) else {
        return fail(status::INVALID_ARGUMENT);
    };
    let Some(collection) = str_of(collection) else {
        return fail(status::INVALID_ARGUMENT);
    };
    // Through the shared `LogicalStore` trait, where `count` lives.
    match db.count(&collection) {
        Ok(n) => {
            unsafe {
                if let Some(s) = out_status.as_mut() {
                    *s = status::OK;
                }
            }
            n as c_longlong
        }
        Err(e) => fail(code_of(&e)),
    }
}

fn str_of(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(p) };
    s.to_str().ok().map(String::from)
}

/// The exported functions declared exactly as a C consumer declares them —
/// C types, C names, no Rust wrappers in sight. The contract tests in
/// `tests/abi.rs` call through this module rather than the safe functions
/// above, so a drift between the header, these declarations, and the
/// implementations fails the build instead of a user.
pub mod raw {
    use std::ffi::{c_char, c_int, c_longlong, c_ulonglong};

    unsafe extern "C" {
        pub safe fn adabt_db_open(path: *const c_char) -> *mut core::ffi::c_void;
        pub safe fn adabt_db_close(db: *mut core::ffi::c_void);
        pub safe fn adabt_db_create_collection(
            db: *mut core::ffi::c_void,
            name: *const c_char,
        ) -> c_int;
        pub safe fn adabt_db_put_i64(
            db: *mut core::ffi::c_void,
            collection: *const c_char,
            id: c_ulonglong,
            field: *const c_char,
            value: i64,
        ) -> c_int;
        pub safe fn adabt_db_get_i64(
            db: *mut core::ffi::c_void,
            collection: *const c_char,
            id: c_ulonglong,
            field: *const c_char,
            out: *mut i64,
        ) -> c_int;
        pub safe fn adabt_db_count(
            db: *mut core::ffi::c_void,
            collection: *const c_char,
            out_status: *mut c_int,
        ) -> c_longlong;
    }
}
