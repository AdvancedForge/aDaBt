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
//! # Shape
//!
//! Each exported function is a thin shim: it hands a closure returning
//! `Result<T, Status>` to `boundary`, which runs it with panics caught and
//! turns the outcome into the C return convention. All logic lives in the
//! closures, written with `?` against `handle` and `text`; no panic and
//! no Rust error ever unwinds into the caller.
//!
//! # Contract
//!
//! - Pointers returned by `adabt_db_open` belong to the caller until passed
//!   to `adabt_db_close`. Passing any other pointer is undefined.
//! - Strings passed in must be valid NUL-terminated UTF-8 and stay alive
//!   for the duration of the call.
//! - One thread at a time per handle: a handle is a single connection, and
//!   two threads sharing one race each other's writes.
//! - An internal panic is reported as `ADABT_INTERNAL` (or NULL / -1). The
//!   handle should be closed afterwards; its in-memory state is not trusted.

use adabt_core::error::Error;
use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_engine::database::Database;
use std::ffi::{c_char, c_int, c_longlong, c_ulonglong, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

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

/// A non-OK status code: the error type every shim body returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Status(c_int);

impl Status {
    const INVALID_ARGUMENT: Status = Status(status::INVALID_ARGUMENT);
    const NOT_FOUND: Status = Status(status::NOT_FOUND);
    const INTERNAL: Status = Status(status::INTERNAL);
}

impl From<Error> for Status {
    fn from(e: Error) -> Self {
        Status(match e {
            Error::NoSuchCollection(_) => status::NO_SUCH_COLLECTION,
            Error::CollectionExists(_) => status::COLLECTION_EXISTS,
            _ => status::INTERNAL,
        })
    }
}

/// Run `body` with panics caught, so nothing unwinds across `extern "C"`.
/// A panic becomes `Status::INTERNAL`.
fn boundary<T>(body: impl FnOnce() -> Result<T, Status>) -> Result<T, Status> {
    // The closure only ever touches the caller's handle, which the contract
    // says is not to be trusted after a panic; asserting unwind safety is
    // therefore sound.
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(Err(Status::INTERNAL))
}

/// The status-code convention: `OK` or the error's code.
fn to_code(outcome: Result<(), Status>) -> c_int {
    match outcome {
        Ok(()) => status::OK,
        Err(Status(code)) => code,
    }
}

/// Borrow a live handle.
///
/// # Safety
/// `db` must be NULL or a live pointer from `adabt_db_open`.
unsafe fn handle<'a>(db: *mut Database) -> Result<&'a mut Database, Status> {
    unsafe { db.as_mut() }.ok_or(Status::INVALID_ARGUMENT)
}

/// Borrow a C string as UTF-8.
///
/// # Safety
/// `p` must be NULL or a valid NUL-terminated string alive for `'a`.
unsafe fn text<'a>(p: *const c_char) -> Result<&'a str, Status> {
    if p.is_null() {
        return Err(Status::INVALID_ARGUMENT);
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| Status::INVALID_ARGUMENT)
}

/// Write through an optional out-pointer.
///
/// # Safety
/// `out` must be NULL or writable for one `T`.
unsafe fn write_out<T>(out: *mut T, value: T) {
    if let Some(slot) = unsafe { out.as_mut() } {
        *slot = value;
    }
}

/// Open (or create) a database directory. Returns NULL on failure.
///
/// # Safety
///
/// `path` must be NULL or a valid NUL-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn adabt_db_open(path: *const c_char) -> *mut Database {
    boundary(|| {
        let path = unsafe { text(path) }?;
        let db = Database::open(Path::new(path), Policy::manual(0))?;
        Ok(Box::into_raw(Box::new(db)))
    })
    .unwrap_or(std::ptr::null_mut())
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
    if db.is_null() {
        return;
    }
    // Nothing to report through `void`; a panicking drop must still not unwind.
    let _ = boundary(|| {
        drop(unsafe { Box::from_raw(db) });
        Ok(())
    });
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
    to_code(boundary(|| {
        let (db, name) = unsafe { (handle(db)?, text(name)?) };
        Ok(db.create_collection(name, Schema::dynamic())?)
    }))
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
    to_code(boundary(|| {
        let (db, collection, field) = unsafe { (handle(db)?, text(collection)?, text(field)?) };
        Ok(db.insert(collection, RecordId(id), Record::new().with(field, value))?)
    }))
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
    to_code(boundary(|| {
        if out.is_null() {
            return Err(Status::INVALID_ARGUMENT);
        }
        let (db, collection, field) = unsafe { (handle(db)?, text(collection)?, text(field)?) };
        let record = db.get(collection, RecordId(id))?.ok_or(Status::NOT_FOUND)?;
        let Some(&Value::I64(v)) = record.get(field) else {
            return Err(Status::NOT_FOUND);
        };
        unsafe { write_out(out, v) };
        Ok(())
    }))
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
    let outcome = boundary(|| {
        let (db, collection) = unsafe { (handle(db)?, text(collection)?) };
        Ok(db.count(collection)?)
    });
    let (code, n) = match outcome {
        Ok(n) => (status::OK, n as c_longlong),
        Err(Status(code)) => (code, -1),
    };
    unsafe { write_out(out_status, code) };
    n
}

/// The exported functions declared exactly as a C consumer declares them —
/// C types, C names, no Rust wrappers in sight. The contract tests in
/// `tests/abi.rs` call through this module rather than the functions above,
/// so a drift between the header, these declarations, and the
/// implementations fails the build instead of a user.
///
/// They are `unsafe` for the same reason the C functions are: they take raw
/// pointers the callee dereferences.
pub mod raw {
    use std::ffi::{c_char, c_int, c_longlong, c_ulonglong, c_void};

    extern "C" {
        pub fn adabt_db_open(path: *const c_char) -> *mut c_void;
        pub fn adabt_db_close(db: *mut c_void);
        pub fn adabt_db_create_collection(db: *mut c_void, name: *const c_char) -> c_int;
        pub fn adabt_db_put_i64(
            db: *mut c_void,
            collection: *const c_char,
            id: c_ulonglong,
            field: *const c_char,
            value: i64,
        ) -> c_int;
        pub fn adabt_db_get_i64(
            db: *mut c_void,
            collection: *const c_char,
            id: c_ulonglong,
            field: *const c_char,
            out: *mut i64,
        ) -> c_int;
        pub fn adabt_db_count(
            db: *mut c_void,
            collection: *const c_char,
            out_status: *mut c_int,
        ) -> c_longlong;
    }
}
