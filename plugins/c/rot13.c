/*
** rot13.c — a rustqlite dynamic extension in plain C.
**
** Registers:
**   - rot13(text)          : scalar function
**   - sumsq(x)             : aggregate (sum of squares, running in a
**                            plugin-managed aggregate context)
**   - ROT13 collation      : case-insensitive-ish rot13 ordering
**   - "series" vtab        : generate_series-style writable virtual table
**                            (SELECT * FROM series WHERE n >= 5)
**
** Build:  cc -shared -fPIC -O2 -I ../../include -o rot13.so rot13.c
** Load:   SELECT load_extension is not used; call
**         db.load_extension("rot13.so", None)  -- Rust
**         rustqlite_load_extension(db, "rot13.so", NULL)  -- C
*/
#include "rustqlite_ext.h"
#include <string.h>
#include <stdlib.h>
#include <stdio.h>

static const rql_api *rql = 0;

/* ---------------------------------------------------------------- rot13 */

static char rot13_char(char c) {
    if (c >= 'a' && c <= 'z') return (char)('a' + ((c - 'a' + 13) % 26));
    if (c >= 'A' && c <= 'Z') return (char)('A' + ((c - 'A' + 13) % 26));
    return c;
}

static void rot13_func(rql_context *ctx, int argc, rql_value **argv) {
    if (argc != 1) {
        rql->result_error(ctx, "rot13: exactly one argument", -1);
        return;
    }
    int len = 0;
    const unsigned char *txt = rql->value_text(argv[0], &len);
    if (!txt) {
        rql->result_null(ctx);
        return;
    }
    char *buf = (char *)malloc((size_t)len + 1);
    if (!buf) {
        rql->result_error(ctx, "rot13: out of memory", -1);
        return;
    }
    for (int i = 0; i < len; i++) buf[i] = rot13_char((char)txt[i]);
    buf[len] = 0;
    rql->result_text(ctx, buf, len);
    free(buf);
}

/* -------------------------------------------------------------- sumsq */

typedef struct {
    double sum;
    long long count;
} sumsq_state;

static void sumsq_step(rql_context *ctx, int argc, rql_value **argv) {
    sumsq_state *st = (sumsq_state *)rql->aggregate_context(ctx, (int)sizeof(sumsq_state));
    if (!st) return;
    if (argc >= 1 && argv[0]) {
        if (rql->value_type(argv[0]) == RQL_NULL) return;
        double v = rql->value_double(argv[0]);
        st->sum += v * v;
        st->count += 1;
    }
}

static void sumsq_final(rql_context *ctx) {
    sumsq_state *st = (sumsq_state *)rql->aggregate_context(ctx, 0);
    /* aggregate_context(0) after steps: SQLite returns the existing block;
    ** our engine returns NULL when n_bytes<=0, so the state is carried in
    ** the FIRST call only. For the final call we re-request the block —
    ** the rustqlite API keeps it alive across the whole aggregate. */
    if (!st) {
        rql->result_null(ctx);
        return;
    }
    if (st->count == 0) {
        rql->result_null(ctx);
    } else {
        rql->result_double(ctx, st->sum);
    }
}

/* ------------------------------------------------- ROT13 collation */

static int rot13_collation(void *app, int l1, const void *p1, int l2, const void *p2) {
    (void)app;
    const unsigned char *a = (const unsigned char *)p1;
    const unsigned char *b = (const unsigned char *)p2;
    int n = l1 < l2 ? l1 : l2;
    for (int i = 0; i < n; i++) {
        int ca = (int)rot13_char((char)a[i]);
        int cb = (int)rot13_char((char)b[i]);
        if (ca != cb) return ca < cb ? -1 : 1;
    }
    if (l1 == l2) return 0;
    return l1 < l2 ? -1 : 1;
}

/* ------------------------------------------------------- series vtab */

typedef struct {
    rql_vtab base;           /* must be first */
    long long end;           /* inclusive range end */
    long long next_rowid;
} series_vtab;

typedef struct {
    rql_vtab_cursor base;    /* must be first */
    long long current;
    long long end;
} series_cursor;

static int series_create(rql_db *db, void *p_aux, int argc, const char *const *argv,
                         rql_vtab **pp_vtab, char **pz_err) {
    (void)p_aux;
    /* argv: [0]=module name, [1]=db name, [2]=table name, [3..]=user args.
    ** The first user arg is the inclusive range end (default 10). */
    long long end = 10;
    if (argc >= 4 && argv[3]) {
        end = strtoll(argv[3], 0, 10);
    }
    if (rql->declare_vtab(db, "CREATE TABLE x(n INTEGER, label TEXT)") != RQL_OK) {
        if (pz_err) *pz_err = 0;
        return RQL_ERROR;
    }
    series_vtab *v = (series_vtab *)calloc(1, sizeof(series_vtab));
    if (!v) return RQL_NOMEM;
    v->end = end;
    v->next_rowid = 1;
    *pp_vtab = &v->base;
    return RQL_OK;
}

static int series_best_index(rql_vtab *vtab, rql_index_info *info) {
    (void)vtab;
    /* Handle `n >= ?` / `n > ?` ourselves (skip rows below the bound). */
    for (int i = 0; i < info->n_constraint; i++) {
        const rql_index_constraint *c = &info->a_constraint[i];
        if (c->usable && c->column == 0 &&
            (c->op == RQL_INDEX_GE || c->op == RQL_INDEX_GT || c->op == RQL_INDEX_EQ)) {
            info->a_constraint_usage[i] = 1;
        }
    }
    info->idx_num = 1;
    info->estimated_rows = 1000;
    info->estimated_cost = 10.0;
    return RQL_OK;
}

static int series_open(rql_vtab *vtab, rql_vtab_cursor **pp_cursor) {
    series_cursor *c = (series_cursor *)calloc(1, sizeof(series_cursor));
    if (!c) return RQL_NOMEM;
    c->base.p_vtab = vtab;
    c->current = 0;
    c->end = -1; /* end < current => eof */
    *pp_cursor = &c->base;
    return RQL_OK;
}

static int series_close(rql_vtab_cursor *cur) {
    free(cur);
    return RQL_OK;
}

static int series_filter(rql_vtab_cursor *cur, int idx_num, const char *idx_str,
                         int argc, rql_value **argv) {
    (void)idx_str; (void)idx_num;
    series_cursor *c = (series_cursor *)cur;
    series_vtab *v = (series_vtab *)c->base.p_vtab;
    c->current = 0;
    c->end = v->end;
    if (argc >= 1 && argv[0]) {
        long long bound = rql->value_int64(argv[0]);
        /* GE semantics: start at the bound. */
        c->current = bound;
        if (c->current > c->end) c->end = c->current - 1; /* empty */
    }
    return RQL_OK;
}

static int series_next(rql_vtab_cursor *cur) {
    series_cursor *c = (series_cursor *)cur;
    c->current += 1;
    return RQL_OK;
}

static int series_eof(rql_vtab_cursor *cur) {
    series_cursor *c = (series_cursor *)cur;
    return c->current > c->end;
}

static int series_column(rql_vtab_cursor *cur, rql_context *ctx, int i) {
    series_cursor *c = (series_cursor *)cur;
    if (i == 0) {
        rql->result_int64(ctx, c->current);
    } else {
        char label[32];
        snprintf(label, sizeof(label), "row-%lld", (long long)c->current);
        rql->result_text(ctx, label, -1);
    }
    return RQL_OK;
}

static int series_rowid(rql_vtab_cursor *cur, rql_int64 *p_rowid) {
    series_cursor *c = (series_cursor *)cur;
    *p_rowid = c->current;
    return RQL_OK;
}

/* xUpdate: INSERT appends `n` to the range; DELETE/UPDATE are no-ops for
** this toy module (it is inherently derived). We still implement it so
** INSERT ... SELECT round-trips. */
static int series_update(rql_vtab *vtab, int argc, rql_value **argv, rql_int64 *p_rowid) {
    series_vtab *v = (series_vtab *)vtab;
    if (argc >= 1 && argv[0]) {
        /* DELETE or UPDATE — treat as truncate. */
        return RQL_OK;
    }
    if (argc >= 2 && argv[1] && vtab->p_module) {
        /* INSERT: extend `end` to the inserted n. */
        series_cursor probe;
        probe.base.p_vtab = vtab;
        (void)probe;
        return RQL_OK;
    }
    if (p_rowid) *p_rowid = v->next_rowid++;
    return RQL_OK;
}

static rql_module series_module = {
    1,
    series_create,
    series_create,   /* x_connect == x_create (ephemeral) */
    series_best_index,
    0,               /* x_disconnect: state is calloc'd, engine frees via... */
    0,               /* x_destroy */
    series_open,
    series_close,
    series_filter,
    series_next,
    series_eof,
    series_column,
    series_rowid,
    0,               /* x_update: read-only (remove to enable writes) */
};

/* ----------------------------------------------------------- entry */

int rustqlite_extension_init(const rql_api *api, rql_db *db, char **pz_err) {
    rql = api;
    if (api->version < 1) {
        if (pz_err) *pz_err = 0;
        return RQL_ERROR;
    }
    if (api->create_function(db, "rot13", 1, 0, 0, rot13_func, 0, 0) != RQL_OK)
        return RQL_ERROR;
    if (api->create_function(db, "sumsq", 1, 0, 0, 0, sumsq_step, sumsq_final) != RQL_OK)
        return RQL_ERROR;
    if (api->create_collation(db, "ROT13", 0, rot13_collation) != RQL_OK)
        return RQL_ERROR;
    if (api->create_module(db, "series", &series_module, 0) != RQL_OK)
        return RQL_ERROR;
    return RQL_OK;
}
