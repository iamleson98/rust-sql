/*
** rustqlite_ext.h — C ABI for rustqlite dynamic extensions.
**
** Works from C, C++, Zig (via C import), and Rust (cdylib with `extern "C"`).
** The protocol mirrors SQLite's loadable-extension API:
**
**   1. Compile your plugin to a shared library.
**   2. Export `rustqlite_extension_init`.
**   3. Call `api->create_function / create_collation / create_module`
**      from the entry point.
**   4. Load it: `db.load_extension("myplugin.so", None)` (Rust) or
**      `rustqlite_load_extension(db, "myplugin.so", 0)` (C).
**
** Status codes match SQLite: RQL_OK=0, RQL_ERROR=1, RQL_ROW=100,
** RQL_DONE=101. Value type codes match too (1=INTEGER, 2=FLOAT, 3=TEXT,
** 4=BLOB, 5=NULL).
*/
#ifndef RUSTQLITE_EXT_H
#define RUSTQLITE_EXT_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#define RQL_OK       0
#define RQL_ERROR    1
#define RQL_NOMEM    2
#define RQL_MISUSE  21
#define RQL_ROW    100
#define RQL_DONE   101

#define RQL_INTEGER 1
#define RQL_FLOAT   2
#define RQL_TEXT    3
#define RQL_BLOB    4
#define RQL_NULL    5

/* xBestIndex constraint operators (SQLite values). */
#define RQL_INDEX_EQ    2
#define RQL_INDEX_GT    4
#define RQL_INDEX_LE    8
#define RQL_INDEX_LT   16
#define RQL_INDEX_GE   32
#define RQL_INDEX_LIKE 66
#define RQL_INDEX_GLOB 74

typedef long long rql_int64;

typedef struct rql_api         rql_api;
typedef struct rql_db          rql_db;
typedef struct rql_value       rql_value;
typedef struct rql_context     rql_context;
typedef struct rql_module      rql_module;
typedef struct rql_vtab        rql_vtab;
typedef struct rql_vtab_cursor rql_vtab_cursor;
typedef struct rql_index_info  rql_index_info;

/* --- function callbacks (SQLite shapes) --- */
typedef void (*rql_func_fn)(rql_context *ctx, int argc, rql_value **argv);
typedef void (*rql_final_fn)(rql_context *ctx);
typedef int  (*rql_collation_fn)(void *p_app, int len1, const void *p1,
                                 int len2, const void *p2);
typedef void (*rql_destructor_fn)(void *);

/* --- virtual table module --- */
struct rql_vtab {
    const rql_module *p_module; /* filled by the engine */
    void             *p_aux;    /* your create_module p_aux */
    char             *z_err_msg;/* set with rql_api->malloc; engine frees */
};

struct rql_vtab_cursor {
    rql_vtab *p_vtab;           /* your cursor struct starts with this */
};

typedef struct rql_index_constraint {
    int column;                 /* column index, -1 = rowid */
    int op;                     /* RQL_INDEX_* */
    unsigned char usable;
} rql_index_constraint;

struct rql_index_info {
    int n_constraint;
    const rql_index_constraint *a_constraint;
    /* outputs */
    int    idx_num;             /* strategy id, passed back to x_filter */
    char  *idx_str;             /* strategy string (rql_api->malloc), engine frees */
    unsigned char *a_constraint_usage; /* 1 per constraint you handle */
    double estimated_cost;
    rql_int64 estimated_rows;
};

struct rql_module {
    int i_version;
    int (*x_create)(rql_db*, void *p_aux, int argc, const char *const*argv,
                    rql_vtab **pp_vtab, char **pz_err);
    int (*x_connect)(rql_db*, void *p_aux, int argc, const char *const*argv,
                     rql_vtab **pp_vtab, char **pz_err);
    int (*x_best_index)(rql_vtab*, rql_index_info*);
    int (*x_disconnect)(rql_vtab*);
    int (*x_destroy)(rql_vtab*);
    int (*x_open)(rql_vtab*, rql_vtab_cursor**);
    int (*x_close)(rql_vtab_cursor*);
    int (*x_filter)(rql_vtab_cursor*, int idx_num, const char *idx_str,
                    int argc, rql_value **argv);
    int (*x_next)(rql_vtab_cursor*);
    int (*x_eof)(rql_vtab_cursor*);          /* returns 1 at end of scan */
    int (*x_column)(rql_vtab_cursor*, rql_context *ctx, int i);
    int (*x_rowid)(rql_vtab_cursor*, rql_int64 *p_rowid);
    /* xUpdate protocol (SQLite): argv[0] = old rowid (NULL = insert);
    ** argv[1..n_cols] = new values (NULL = unchanged). argc==1 with a
    ** non-NULL argv[0] and argc beyond 1 absent = delete. Provide NULL
    ** for read-only tables. */
    int (*x_update)(rql_vtab*, int argc, rql_value **argv, rql_int64 *p_rowid);
};

/* --- the API table handed to your entry point --- */
struct rql_api {
    int version;                            /* 1 */
    /* results */
    void (*result_int64)(rql_context*, rql_int64);
    void (*result_double)(rql_context*, double);
    void (*result_text)(rql_context*, const char*, int n);  /* n<0: strlen; copied */
    void (*result_blob)(rql_context*, const void*, int n);
    void (*result_null)(rql_context*);
    void (*result_error)(rql_context*, const char*, int n);
    /* value access */
    int  (*value_type)(rql_value*);
    rql_int64 (*value_int64)(rql_value*);
    double (*value_double)(rql_value*);
    const unsigned char *(*value_text)(rql_value*, int *plen);
    const void *(*value_blob)(rql_value*, int *plen);
    int  (*value_bytes)(rql_value*);
    /* aggregates */
    void *(*aggregate_context)(rql_context*, int n_bytes); /* zeroed once */
    /* registration */
    int  (*create_function)(rql_db*, const char *name, int n_arg,
                             int e_text_rep, void *p_app,
                             rql_func_fn x_func, rql_func_fn x_step,
                             rql_final_fn x_final);
    int  (*create_collation)(rql_db*, const char *name, void *p_app,
                             rql_collation_fn x_compare);
    int  (*create_module)(rql_db*, const char *name, const rql_module*, void *p_aux);
    int  (*declare_vtab)(rql_db*, const char *create_table_sql);
    /* misc */
    int  (*exec)(rql_db*, const char *sql);
    const char *(*errmsg)(rql_db*);
    void *(*malloc)(size_t);
    void  (*free)(void *);
    const char *(*engine_version)(void);
};

/* The entry point every rustqlite extension exports. */
typedef int (*rql_ext_init_fn)(const rql_api *api, rql_db *db, char **pz_err);
int rustqlite_extension_init(const rql_api *api, rql_db *db, char **pz_err);

/* SQLITE_STATIC / SQLITE_TRANSIENT equivalents for bind_text. */
#define RQL_STATIC    ((rql_destructor_fn)0)
#define RQL_TRANSIENT ((rql_destructor_fn)-1)

#ifdef __cplusplus
}
#endif

#endif /* RUSTQLITE_EXT_H */
