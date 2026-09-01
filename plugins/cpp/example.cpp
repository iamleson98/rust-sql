// example.cpp — rustqlite dynamic extension in C++.
//
// Registers:
//   - shout(text)      : scalar (uppercases + appends "!")
//   - movavg(x, k)     : aggregate — moving average using plugin-managed
//                        state (last k values in a ring buffer)
//   - NUMERIC collation: orders text by numeric prefix when present
//   - "kvstore" vtab   : a small writable key/value virtual table
//
// Build:
//   c++ -shared -fPIC -O2 -std=c++17 -I ../../include -o example.so example.cpp
//
// This file demonstrates that the C header drives C++ naturally; no
// engine linkage is needed — the extension only talks through the API
// table handed to rustqlite_extension_init.

#include "rustqlite_ext.h"

#include <cstring>
#include <cstdlib>
#include <cstdio>
#include <string>
#include <vector>
#include <deque>

static const rql_api *rql = nullptr;

// ----------------------------------------------------------------- shout

static void shout_func(rql_context *ctx, int argc, rql_value **argv) {
    if (argc != 1) {
        rql->result_error(ctx, "shout: exactly one argument", -1);
        return;
    }
    if (rql->value_type(argv[0]) == RQL_NULL) {
        rql->result_null(ctx);
        return;
    }
    int len = 0;
    const unsigned char *txt = rql->value_text(argv[0], &len);
    std::string s(reinterpret_cast<const char *>(txt), (size_t)len);
    for (auto &c : s) c = (char)toupper((unsigned char)c);
    s += "!";
    rql->result_text(ctx, s.c_str(), (int)s.size());
}

// ---------------------------------------------------------------- movavg
// NOTE: aggregate_context hands back ZEROED RAW memory — a C++ object with
// non-trivial members (std::deque, std::string, ...) must be constructed
// with placement-new on first use and destroyed in xFinal. This example
// keeps the state a POD instead (fixed ring buffer), which is the simpler
// and safer pattern for plugin aggregates.

struct MovAvgState {
    double window[3];
    size_t len;
    size_t next;
    double sum;
};

static void movavg_step(rql_context *ctx, int argc, rql_value **argv) {
    auto *st = (MovAvgState *)rql->aggregate_context(ctx, (int)sizeof(MovAvgState));
    if (!st) return;
    if (argc < 1 || !argv[0] || rql->value_type(argv[0]) == RQL_NULL) return;
    const size_t K = sizeof(st->window) / sizeof(st->window[0]);
    double v = rql->value_double(argv[0]);
    if (st->len < K) {
        st->window[st->len] = v;
        st->len += 1;
        st->sum += v;
    } else {
        // Evict the oldest (ring position), add the new value.
        st->sum -= st->window[st->next];
        st->window[st->next] = v;
        st->sum += v;
        st->next = (st->next + 1) % K;
    }
}

static void movavg_final(rql_context *ctx) {
    auto *st = (MovAvgState *)rql->aggregate_context(ctx, 0);
    if (!st || st->len == 0) {
        rql->result_null(ctx);
        return;
    }
    rql->result_double(ctx, st->sum / (double)st->len);
}

// ------------------------------------------------------- NUMERIC order

static int numeric_collation(void *app, int l1, const void *p1, int l2, const void *p2) {
    (void)app;
    char b1[32], b2[32];
    size_t n1 = (size_t)(l1 < 31 ? l1 : 31), n2 = (size_t)(l2 < 31 ? l2 : 31);
    memcpy(b1, p1, n1); b1[n1] = 0;
    memcpy(b2, p2, n2); b2[n2] = 0;
    char *e1 = nullptr, *e2 = nullptr;
    long v1 = strtol(b1, &e1, 10);
    long v2 = strtol(b2, &e2, 10);
    bool n1_ok = e1 != b1, n2_ok = e2 != b2;
    if (n1_ok && n2_ok && v1 != v2) return v1 < v2 ? -1 : 1;
    if (n1_ok != n2_ok) return n1_ok ? -1 : 1; // numbers sort before text
    int m = l1 < l2 ? l1 : l2;
    int c = memcmp(p1, p2, (size_t)m);
    if (c != 0) return c < 0 ? -1 : 1;
    return l1 == l2 ? 0 : (l1 < l2 ? -1 : 1);
}

// ------------------------------------------------------------ kv vtab

struct KvVtab {
    rql_vtab base;                 // must be first
    std::vector<std::pair<std::string, std::string>> rows;
    long long next_rowid = 1;
};

struct KvCursor {
    rql_vtab_cursor base;          // must be first
    size_t pos = 0;
};

static int kv_create(rql_db *db, void *p_aux, int argc, const char *const *argv,
                     rql_vtab **pp, char **pz_err) {
    (void)p_aux; (void)argc; (void)argv;
    if (rql->declare_vtab(db, "CREATE TABLE x(k TEXT, v TEXT)") != RQL_OK) {
        if (pz_err) *pz_err = nullptr;
        return RQL_ERROR;
    }
    auto *v = new KvVtab();
    *pp = &v->base;
    return RQL_OK;
}

static int kv_disconnect(rql_vtab *vtab) {
    delete (KvVtab *)vtab;
    return RQL_OK;
}

static int kv_best_index(rql_vtab *, rql_index_info *info) {
    for (int i = 0; i < info->n_constraint; i++) {
        const rql_index_constraint *c = &info->a_constraint[i];
        if (c->usable && c->column == 0 && c->op == RQL_INDEX_EQ) {
            info->a_constraint_usage[i] = 1; // we handle k = ?
        }
    }
    info->idx_num = 2;
    info->estimated_rows = 100;
    info->estimated_cost = 5.0;
    return RQL_OK;
}

static int kv_open(rql_vtab *vtab, rql_vtab_cursor **pp) {
    auto *c = new KvCursor();
    c->base.p_vtab = vtab;
    *pp = &c->base;
    return RQL_OK;
}

static int kv_close(rql_vtab_cursor *cur) {
    delete (KvCursor *)cur;
    return RQL_OK;
}

static int kv_filter(rql_vtab_cursor *cur, int idx_num, const char *, int argc, rql_value **argv) {
    auto *c = (KvCursor *)cur;
    auto *v = (KvVtab *)c->base.p_vtab;
    (void)idx_num;
    c->pos = 0;
    if (argc >= 1 && argv[0] && idx_num == 2) {
        int len = 0;
        const unsigned char *k = rql->value_text(argv[0], &len);
        std::string key((const char *)k, (size_t)len);
        // Point-wise lookup: seek to the matching row (or past the end).
        size_t i = 0;
        for (; i < v->rows.size(); i++) {
            if (v->rows[i].first == key) break;
        }
        if (i < v->rows.size()) {
            c->pos = i;
        } else {
            c->pos = v->rows.size() + 1; // eof
        }
    }
    return RQL_OK;
}

static int kv_next(rql_vtab_cursor *cur) {
    ((KvCursor *)cur)->pos += 1;
    return RQL_OK;
}

static int kv_eof(rql_vtab_cursor *cur) {
    auto *c = (KvCursor *)cur;
    auto *v = (KvVtab *)c->base.p_vtab;
    return c->pos >= v->rows.size();
}

static int kv_column(rql_vtab_cursor *cur, rql_context *ctx, int i) {
    auto *c = (KvCursor *)cur;
    auto *v = (KvVtab *)c->base.p_vtab;
    if (c->pos >= v->rows.size()) {
        rql->result_null(ctx);
        return RQL_OK;
    }
    const std::string &s = i == 0 ? v->rows[c->pos].first : v->rows[c->pos].second;
    rql->result_text(ctx, s.c_str(), (int)s.size());
    return RQL_OK;
}

static int kv_rowid(rql_vtab_cursor *cur, rql_int64 *p_rowid) {
    auto *c = (KvCursor *)cur;
    *p_rowid = (rql_int64)(c->pos + 1);
    return RQL_OK;
}

static int kv_update(rql_vtab *vtab, int argc, rql_value **argv, rql_int64 *p_rowid) {
    auto *v = (KvVtab *)vtab;
    // xUpdate protocol (SQLite): argv[0] is the OLD rowid — a NULL VALUE
    // means INSERT, a non-NULL value means UPDATE/DELETE. Always check
    // value_type, never pointer truthiness (the engine passes a non-NULL
    // pointer to a NULL value for the insert case).
    bool is_insert = argc >= 1 && argv[0] && rql->value_type(argv[0]) == RQL_NULL;
    if (!is_insert && argc >= 1 && argv[0]) {
        // DELETE (no new column values) or UPDATE.
        rql_int64 rid = rql->value_int64(argv[0]);
        size_t idx = (size_t)(rid - 1);
        if (argc == 1) {
            if (idx < v->rows.size()) v->rows.erase(v->rows.begin() + (long)idx);
            return RQL_OK;
        }
        if (idx < v->rows.size() && argc >= 3 && argv[1] && argv[2]) {
            int klen = 0, vlen = 0;
            const unsigned char *k = rql->value_text(argv[1], &klen);
            const unsigned char *val = rql->value_text(argv[2], &vlen);
            if (k) v->rows[idx].first.assign((const char *)k, (size_t)klen);
            if (val) v->rows[idx].second.assign((const char *)val, (size_t)vlen);
        }
        return RQL_OK;
    }
    // INSERT: argv[1]=k argv[2]=v.
    if (argc >= 3 && argv[1]) {
        int klen = 0, vlen = 0;
        const unsigned char *k = argv[1] ? rql->value_text(argv[1], &klen) : nullptr;
        const unsigned char *val = argv[2] ? rql->value_text(argv[2], &vlen) : nullptr;
        std::string key((const char *)k, (size_t)klen);
        std::string value((const char *)(val ? val : (const unsigned char *)""), (size_t)(val ? vlen : 0));
        for (auto &row : v->rows) {
            if (row.first == key) {
                row.second = value; // upsert
                if (p_rowid) *p_rowid = (rql_int64)(&row - &v->rows[0] + 1);
                return RQL_OK;
            }
        }
        v->rows.push_back({key, value});
        if (p_rowid) *p_rowid = v->next_rowid + (rql_int64)v->rows.size();
        return RQL_OK;
    }
    return RQL_OK;
}

static rql_module kv_module = {
    1,
    kv_create,
    kv_create,
    kv_best_index,
    kv_disconnect,
    kv_disconnect, // x_destroy
    kv_open,
    kv_close,
    kv_filter,
    kv_next,
    kv_eof,
    kv_column,
    kv_rowid,
    kv_update,
};

// --------------------------------------------------------------- entry

extern "C" int rustqlite_extension_init(const rql_api *api, rql_db *db, char **pz_err) {
    rql = api;
    if (api->version < 1) {
        if (pz_err) *pz_err = nullptr;
        return RQL_ERROR;
    }
    if (api->create_function(db, "shout", 1, 0, nullptr, shout_func, nullptr, nullptr) != RQL_OK)
        return RQL_ERROR;
    if (api->create_function(db, "movavg", 1, 0, nullptr, nullptr, movavg_step, movavg_final) != RQL_OK)
        return RQL_ERROR;
    if (api->create_collation(db, "NUMERIC", nullptr, numeric_collation) != RQL_OK)
        return RQL_ERROR;
    if (api->create_module(db, "kvstore", &kv_module, nullptr) != RQL_OK)
        return RQL_ERROR;
    return RQL_OK;
}
