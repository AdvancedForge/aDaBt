//! Proof that the ABI works for its actual audience: a C compiler.
//!
//! This test does not use Rust's declarations at all. It compiles a small
//! C program against `include/adabt.h`, links it against the built cdylib,
//! and runs it end to end. If the header lies about a signature or a
//! status code — or stops matching what the library exports — this fails
//! in exactly the way a downstream packager's build would.

use std::path::PathBuf;
use std::process::Command;

/// Locate the built cdylib by walking up from this test binary's own
/// directory (`target/<profile>/deps/…`) to the profile directory, where
/// cargo places `libadabt_ffi.<ext>` next to the test executable's crate.
fn find_cdylib() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary location");
    let names = ["libadabt_ffi.so", "libadabt_ffi.dylib", "adabt_ffi.dll"];
    let mut dir: Option<&std::path::Path> = exe.parent();
    while let Some(d) = dir {
        for name in names {
            let candidate = d.join(name);
            if candidate.exists() {
                return candidate;
            }
        }
        dir = d.parent();
    }
    panic!("built cdylib not found above {:?}", exe);
}

fn scratch_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("adabt-c-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

const PROGRAM: &str = r#"
#include <stdio.h>
#include <string.h>
#include "adabt.h"

static int fail(const char *why) { fprintf(stderr, "FAIL: %s\n", why); return 1; }

int main(int argc, char **argv) {
    if (argc != 2) return fail("usage: prog <db-path>");
    adabt_db *db = adabt_db_open(argv[1]);
    if (!db) return fail("open returned NULL");
    if (adabt_db_create_collection(db, "c") != ADABT_OK) return fail("create");
    if (adabt_db_create_collection(db, "c") != ADABT_COLLECTION_EXISTS)
        return fail("duplicate create gave the wrong code");
    if (adabt_db_put_i64(db, "c", 7, "n", -123456789LL) != ADABT_OK)
        return fail("put");
    int64_t v = 0;
    if (adabt_db_get_i64(db, "c", 7, "n", &v) != ADABT_OK) return fail("get");
    if (v != -123456789LL) return fail("round trip lost the value");
    if (adabt_db_get_i64(db, "c", 8, "n", &v) != ADABT_NOT_FOUND)
        return fail("missing record gave the wrong code");
    int32_t st = -1;
    if (adabt_db_count(db, "c", &st) != 1 || st != ADABT_OK) return fail("count");
    if (adabt_db_count(db, "absent", &st) != -1 || st != ADABT_NO_SUCH_COLLECTION)
        return fail("count of absent collection");
    /* NULL arguments must be refused, not crashed on. */
    if (adabt_db_get_i64(db, NULL, 1, NULL, &v) != ADABT_INVALID_ARGUMENT)
        return fail("NULL strings not refused");
    if (adabt_db_get_i64(NULL, "c", 1, "n", &v) != ADABT_INVALID_ARGUMENT)
        return fail("NULL handle not refused");
    adabt_db_close(db);
    adabt_db_close(NULL); /* free(NULL) semantics */
    printf("ok\n");
    return 0;
}
"#;

#[test]
fn a_c_program_compiles_links_and_round_trips() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let header_dir = manifest.join("include");
    assert!(header_dir.join("adabt.h").exists(), "header missing");

    let lib = find_cdylib();
    let lib_dir = lib.parent().unwrap();

    let work = scratch_dir("build");
    let src = work.join("prog.c");
    std::fs::write(&src, PROGRAM).unwrap();
    let bin = work.join("prog");

    let status = Command::new("cc")
        .arg(src.to_str().unwrap())
        .arg(format!("-I{}", header_dir.display()))
        .arg(format!("-L{}", lib_dir.display()))
        .arg(format!("-Wl,-rpath,{}", lib_dir.display()))
        .arg("-ladabt_ffi")
        .arg("-o")
        .arg(bin.to_str().unwrap())
        .status()
        .expect("cc exists on any machine that links Rust programs");
    assert!(status.success(), "the C program did not compile/link");

    let db_path = scratch_dir("data");
    let run = Command::new(&bin)
        .arg(db_path.to_str().unwrap())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "the C program failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
}
