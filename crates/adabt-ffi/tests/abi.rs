//! The C ABI exercised through C-typed declarations.
//!
//! These tests call the exported functions through `adabt_ffi::raw`, whose
//! signatures are written exactly as a C consumer writes them — pointers,
//! c_int codes, no Rust wrappers. If the ABI drifts from
//! `include/adabt.h`, these calls stop compiling or stop behaving before
//! any user notices.

use adabt_ffi::raw::*;
use std::ffi::{c_int, CString};

const OK: c_int = 0;
const NO_SUCH_COLLECTION: c_int = 1;
const COLLECTION_EXISTS: c_int = 2;
const INVALID_ARGUMENT: c_int = 3;
const NOT_FOUND: c_int = 4;

struct Db(*mut core::ffi::c_void);
impl Drop for Db {
    fn drop(&mut self) {
        adabt_db_close(self.0);
    }
}

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn tmpdir(tag: &str) -> CString {
    let p = std::env::temp_dir().join(format!("adabt-ffi-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    cs(p.to_str().unwrap())
}

#[test]
fn open_put_get_count_round_trip_through_c() {
    let dir = tmpdir("roundtrip");
    let db = Db(adabt_db_open(dir.as_ptr()));
    assert!(!db.0.is_null(), "adabt_db_open returned NULL");

    let name = cs("metrics");
    assert_eq!(adabt_db_create_collection(db.0, name.as_ptr()), OK);
    // Duplicate creation is refused with its own code, not a generic one.
    assert_eq!(
        adabt_db_create_collection(db.0, name.as_ptr()),
        COLLECTION_EXISTS
    );

    let coll = cs("metrics");
    let field = cs("temperature");
    assert_eq!(
        adabt_db_put_i64(db.0, coll.as_ptr(), 42, field.as_ptr(), -7),
        OK
    );

    let mut out: i64 = 0;
    assert_eq!(
        adabt_db_get_i64(db.0, coll.as_ptr(), 42, field.as_ptr(), &mut out),
        OK
    );
    assert_eq!(out, -7, "the i64 did not survive the boundary");

    // Missing record, missing collection: distinct codes for distinct lies.
    assert_eq!(
        adabt_db_get_i64(db.0, coll.as_ptr(), 43, field.as_ptr(), &mut out),
        NOT_FOUND
    );
    let absent = cs("nope");
    assert_eq!(
        adabt_db_get_i64(db.0, absent.as_ptr(), 42, field.as_ptr(), &mut out),
        NO_SUCH_COLLECTION
    );

    let mut status: c_int = -1;
    assert_eq!(adabt_db_count(db.0, coll.as_ptr(), &mut status), 1);
    assert_eq!(status, OK);
}

#[test]
fn null_and_garbage_arguments_are_refused_not_crashed() {
    let dir = tmpdir("nulls");
    let db = Db(adabt_db_open(dir.as_ptr()));
    assert!(!db.0.is_null());

    let field = cs("f");
    assert_eq!(
        adabt_db_get_i64(
            db.0,
            field.as_ptr(),
            1,
            field.as_ptr(),
            std::ptr::null_mut(),
        ),
        INVALID_ARGUMENT
    );
    assert_eq!(
        adabt_db_put_i64(db.0, std::ptr::null(), 1, field.as_ptr(), 0),
        INVALID_ARGUMENT
    );

    // A NULL handle is refused cleanly by every function that takes one.
    let mut out: i64 = 0;
    assert_eq!(
        adabt_db_get_i64(
            std::ptr::null_mut(),
            field.as_ptr(),
            1,
            field.as_ptr(),
            &mut out,
        ),
        INVALID_ARGUMENT
    );

    // close(NULL) is free(), not a fault.
    adabt_db_close(std::ptr::null_mut());

    // Opening a path that cannot exist fails to NULL rather than panicking
    // across the boundary.
    let bad = cs("/dev/null/adabt-cannot-exist");
    assert!(adabt_db_open(bad.as_ptr()).is_null());
}
