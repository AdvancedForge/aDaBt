/*
 * adabt.h — the C contract for the adabt-ffi library.
 *
 * Kept in lockstep with `crates/adabt-ffi/src/lib.rs`; that crate's tests
 * pin every status code pair, so a change here without a change there fails
 * the build.
 */
#ifndef ADABT_H
#define ADABT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Status codes. */
enum {
    ADABT_OK = 0,
    ADABT_NO_SUCH_COLLECTION = 1,
    ADABT_COLLECTION_EXISTS = 2,
    ADABT_INVALID_ARGUMENT = 3,
    ADABT_NOT_FOUND = 4,
    ADABT_INTERNAL = 5,
};

/* Opaque database handle. Owned by the caller between open and close. */
typedef struct adabt_db adabt_db;

/* Open (or create) a database directory. NULL on failure. */
adabt_db *adabt_db_open(const char *path);

/* Close a handle from adabt_db_open. NULL is accepted and ignored. */
void adabt_db_close(adabt_db *db);

/* Create a dynamic-schema collection. ADABT_COLLECTION_EXISTS on repeat. */
int32_t adabt_db_create_collection(adabt_db *db, const char *name);

/* Set one i64 field, creating or replacing record `id` with that field. */
int32_t adabt_db_put_i64(adabt_db *db, const char *collection,
                         uint64_t id, const char *field, int64_t value);

/* Read one i64 field. *out written only on ADABT_OK. */
int32_t adabt_db_get_i64(adabt_db *db, const char *collection,
                         uint64_t id, const char *field, int64_t *out);

/* Live count; -1 with *out_status set on error. out_status may be NULL. */
int64_t adabt_db_count(adabt_db *db, const char *collection,
                       int32_t *out_status);

#ifdef __cplusplus
}
#endif

#endif /* ADABT_H */
