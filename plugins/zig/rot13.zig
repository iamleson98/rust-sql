//! rot13.zig — a rustqlite dynamic extension in Zig.
//!
//! Registers:
//!   - rot13(text)   : scalar function
//!   - zcount(x)     : aggregate (count of non-NULL values)
//!   - ZREVERSE collation : compares reversed byte sequences
//!   - "range" vtab  : SELECT * FROM range(10) — numbers 0..n-1
//!
//! Build (Zig 0.13):
//!   zig build-lib -dynamic -O ReleaseFast -I ../../include rot13.zig
//! Produces librot13.so — load with `db.load_extension("librot13.so", None)`.

const std = @import("std");

// The C ABI types, declared natively (avoids @cImport of the header; the
// definitions are layout-compatible with include/rustqlite_ext.h).
const rql_int64 = i64;
// Zig's builtin CChar is i8; our ABI is byte-oriented (u8).
const CChar = u8;

pub const RQL_OK: c_int = 0;
pub const RQL_ERROR: c_int = 1;
pub const RQL_NOMEM: c_int = 2;
pub const RQL_MISUSE: c_int = 21;
pub const RQL_INTEGER: c_int = 1;
pub const RQL_FLOAT: c_int = 2;
pub const RQL_TEXT: c_int = 3;
pub const RQL_BLOB: c_int = 4;
pub const RQL_NULL: c_int = 5;
pub const RQL_INDEX_EQ: c_int = 2;
pub const RQL_INDEX_GE: c_int = 32;

pub const RqlValue = opaque {};
pub const RqlContext = opaque {};
pub const RqlDb = opaque {};
pub const RqlVtab = opaque {};
pub const RqlVtabCursor = opaque {};

pub const RqlIndexConstraint = extern struct {
    column: c_int,
    op: c_int,
    usable: u8,
    _pad: [3]u8 = .{ 0, 0, 0 },
};

pub const RqlIndexInfo = extern struct {
    n_constraint: c_int,
    a_constraint: [*]const RqlIndexConstraint,
    idx_num: c_int,
    idx_str: ?[*]CChar,
    a_constraint_usage: ?[*]u8,
    estimated_cost: f64,
    estimated_rows: rql_int64,
};

pub const FuncFn = *const fn (?*RqlContext, c_int, ?[*]?*RqlValue) callconv(.C) void;
pub const FinalFn = *const fn (?*RqlContext) callconv(.C) void;
pub const CollationFn = *const fn (?*anyopaque, c_int, ?*const anyopaque, c_int, ?*const anyopaque) callconv(.C) c_int;

pub const RqlModule = extern struct {
    i_version: c_int,
    x_create: ?*const fn (?*RqlDb, ?*anyopaque, c_int, ?[*]const ?[*:0]const CChar, ?*?*RqlVtab, ?*?[*:0]CChar) callconv(.C) c_int = null,
    x_connect: ?*const fn (?*RqlDb, ?*anyopaque, c_int, ?[*]const ?[*:0]const CChar, ?*?*RqlVtab, ?*?[*:0]CChar) callconv(.C) c_int = null,
    x_best_index: ?*const fn (?*RqlVtab, ?*RqlIndexInfo) callconv(.C) c_int = null,
    x_disconnect: ?*const fn (?*RqlVtab) callconv(.C) c_int = null,
    x_destroy: ?*const fn (?*RqlVtab) callconv(.C) c_int = null,
    x_open: ?*const fn (?*RqlVtab, ?*?*RqlVtabCursor) callconv(.C) c_int = null,
    x_close: ?*const fn (?*RqlVtabCursor) callconv(.C) c_int = null,
    x_filter: ?*const fn (?*RqlVtabCursor, c_int, ?[*:0]const CChar, c_int, ?[*]?*RqlValue) callconv(.C) c_int = null,
    x_next: ?*const fn (?*RqlVtabCursor) callconv(.C) c_int = null,
    x_eof: ?*const fn (?*RqlVtabCursor) callconv(.C) c_int = null,
    x_column: ?*const fn (?*RqlVtabCursor, ?*RqlContext, c_int) callconv(.C) c_int = null,
    x_rowid: ?*const fn (?*RqlVtabCursor, ?*rql_int64) callconv(.C) c_int = null,
    x_update: ?*const fn (?*RqlVtab, c_int, ?[*]?*RqlValue, ?*rql_int64) callconv(.C) c_int = null,
};

pub const RqlApi = extern struct {
    version: c_int,
    result_int64: *const fn (?*RqlContext, rql_int64) callconv(.C) void,
    result_double: *const fn (?*RqlContext, f64) callconv(.C) void,
    result_text: *const fn (?*RqlContext, ?[*]const CChar, c_int) callconv(.C) void,
    result_blob: *const fn (?*RqlContext, ?*const anyopaque, c_int) callconv(.C) void,
    result_null: *const fn (?*RqlContext) callconv(.C) void,
    result_error: *const fn (?*RqlContext, ?[*]const CChar, c_int) callconv(.C) void,
    value_type: *const fn (?*RqlValue) callconv(.C) c_int,
    value_int64: *const fn (?*RqlValue) callconv(.C) rql_int64,
    value_double: *const fn (?*RqlValue) callconv(.C) f64,
    value_text: *const fn (?*RqlValue, ?*c_int) callconv(.C) ?[*]const CChar,
    value_blob: *const fn (?*RqlValue, ?*c_int) callconv(.C) ?*const anyopaque,
    value_bytes: *const fn (?*RqlValue) callconv(.C) c_int,
    aggregate_context: *const fn (?*RqlContext, c_int) callconv(.C) ?*anyopaque,
    create_function: *const fn (?*RqlDb, ?[*:0]const CChar, c_int, c_int, ?*anyopaque, ?FuncFn, ?FuncFn, ?FinalFn) callconv(.C) c_int,
    create_collation: *const fn (?*RqlDb, ?[*:0]const CChar, ?*anyopaque, CollationFn) callconv(.C) c_int,
    create_module: *const fn (?*RqlDb, ?[*:0]const CChar, ?*const RqlModule, ?*anyopaque) callconv(.C) c_int,
    declare_vtab: *const fn (?*RqlDb, ?[*:0]const CChar) callconv(.C) c_int,
    exec: *const fn (?*RqlDb, ?[*:0]const CChar) callconv(.C) c_int,
    errmsg: *const fn (?*RqlDb) callconv(.C) ?[*:0]const CChar,
    malloc: *const fn (usize) callconv(.C) ?*anyopaque,
    free: *const fn (?*anyopaque) callconv(.C) void,
    engine_version: *const fn () callconv(.C) ?[*:0]const CChar,
};

var rql: *const RqlApi = undefined;

// ---------------------------------------------------------------- rot13

fn rot13c(c: u8) u8 {
    return switch (c) {
        'a'...'z' => 'a' + @as(u8, (c - 'a' + 13) % 26),
        'A'...'Z' => 'A' + @as(u8, (c - 'A' + 13) % 26),
        else => c,
    };
}

fn rot13Func(ctx: ?*RqlContext, argc: c_int, argv: ?[*]?*RqlValue) callconv(.C) void {
    if (argc != 1) {
        rql.result_error(ctx, "rot13: one argument", -1);
        return;
    }
    const v = argv.?[0] orelse {
        rql.result_null(ctx);
        return;
    };
    if (rql.value_type(v) == RQL_NULL) {
        rql.result_null(ctx);
        return;
    }
    var len: c_int = 0;
    const txt = rql.value_text(v, &len) orelse {
        rql.result_null(ctx);
        return;
    };
    const buf_len: usize = @intCast(len);
    const buf = std.heap.c_allocator.alloc(u8, buf_len) catch {
        rql.result_error(ctx, "rot13: oom", -1);
        return;
    };
    defer std.heap.c_allocator.free(buf);
    for (0..buf_len) |i| {
        buf[i] = rot13c(txt[i]);
    }
    rql.result_text(ctx, buf.ptr, len);
}

// --------------------------------------------------------------- zcount

const ZCount = extern struct {
    count: rql_int64 = 0,
};

fn zcountStep(ctx: ?*RqlContext, argc: c_int, argv: ?[*]?*RqlValue) callconv(.C) void {
    const st = @as(?*ZCount, @ptrCast(@alignCast(rql.aggregate_context(ctx, @sizeOf(ZCount))))) orelse return;
    if (argc >= 1) {
        if (argv.?[0]) |v| {
            if (rql.value_type(v) != RQL_NULL) st.count += 1;
        }
    }
}

fn zcountFinal(ctx: ?*RqlContext) callconv(.C) void {
    const st = @as(?*ZCount, @ptrCast(@alignCast(rql.aggregate_context(ctx, 0))));
    if (st) |s| {
        rql.result_int64(ctx, s.count);
    } else {
        rql.result_int64(ctx, 0);
    }
}

// -------------------------------------------------------- ZREVERSE

fn zreverseCollation(_: ?*anyopaque, l1: c_int, p1: ?*const anyopaque, l2: c_int, p2: ?*const anyopaque) callconv(.C) c_int {
    const a = @as([*]const u8, @ptrCast(p1 orelse return 0))[0..@intCast(l1)];
    const b = @as([*]const u8, @ptrCast(p2 orelse return 0))[0..@intCast(l2)];
    // Compare from the end backwards.
    var i: usize = a.len;
    var j: usize = b.len;
    while (i > 0 and j > 0) {
        i -= 1;
        j -= 1;
        if (a[i] != b[j]) return if (a[i] < b[j]) -1 else 1;
    }
    if (a.len == b.len) return 0;
    return if (a.len < b.len) -1 else 1;
}

// ----------------------------------------------------------- range vtab

const RangeVtab = extern struct {
    // The engine-known header comes first (p_module/p_aux/z_err_msg).
    p_module: ?*const RqlModule,
    p_aux: ?*anyopaque,
    z_err_msg: ?[*:0]CChar,
    end: rql_int64, // exclusive
};

const RangeCursor = extern struct {
    p_vtab: ?*RqlVtab,
    current: rql_int64,
    stop: rql_int64,
};

fn rangeCreate(db: ?*RqlDb, _: ?*anyopaque, argc: c_int, argv: ?[*]const ?[*:0]const u8, pp: ?*?*RqlVtab, _: ?*?[*:0]u8) callconv(.C) c_int {
    _ = argc;
    if (rql.declare_vtab(db, "CREATE TABLE x(n INTEGER)") != RQL_OK) return RQL_ERROR;
    var end: rql_int64 = 10;
    if (argv) |a| {
        // argv[0]=module, argv[1]=db, argv[2]=table, argv[3..]=user args.
        if (a[3]) |arg| {
            end = std.fmt.parseInt(rql_int64, std.mem.span(@as([*:0]const u8, @ptrCast(arg))), 10) catch 10;
        }
    }
    const v = std.heap.c_allocator.create(RangeVtab) catch return RQL_NOMEM;
    v.* = .{ .p_module = null, .p_aux = null, .z_err_msg = null, .end = end };
    pp.?.* = @ptrCast(v);
    return RQL_OK;
}

fn rangeBestIndex(_: ?*RqlVtab, info: ?*RqlIndexInfo) callconv(.C) c_int {
    const i = info orelse return RQL_ERROR;
    var j: usize = 0;
    while (j < @as(usize, @intCast(i.n_constraint))) : (j += 1) {
        const c = &i.a_constraint[j];
        if (c.usable == 1 and c.column == 0 and (c.op == RQL_INDEX_GE or c.op == RQL_INDEX_EQ)) {
            i.a_constraint_usage.?[j] = 1;
        }
    }
    i.idx_num = 1;
    i.estimated_rows = 100;
    i.estimated_cost = 10.0;
    return RQL_OK;
}

fn rangeOpen(vtab: ?*RqlVtab, pp: ?*?*RqlVtabCursor) callconv(.C) c_int {
    const c = std.heap.c_allocator.create(RangeCursor) catch return RQL_NOMEM;
    const v: *RangeVtab = @ptrCast(@alignCast(vtab.?));
    c.* = .{ .p_vtab = vtab, .current = 0, .stop = v.end };
    pp.?.* = @ptrCast(c);
    return RQL_OK;
}

fn rangeClose(cur: ?*RqlVtabCursor) callconv(.C) c_int {
    std.heap.c_allocator.destroy(@as(*RangeCursor, @ptrCast(@alignCast(cur.?))));
    return RQL_OK;
}

fn rangeFilter(cur: ?*RqlVtabCursor, idx_num: c_int, _: ?[*:0]const CChar, argc: c_int, argv: ?[*]?*RqlValue) callconv(.C) c_int {
    const c: *RangeCursor = @ptrCast(@alignCast(cur.?));
    const v: *RangeVtab = @ptrCast(@alignCast(c.p_vtab.?));
    c.current = 0;
    c.stop = v.end;
    if (idx_num == 1 and argc >= 1) {
        if (argv.?[0]) |arg| {
            c.current = rql.value_int64(arg);
        }
    }
    return RQL_OK;
}

fn rangeNext(cur: ?*RqlVtabCursor) callconv(.C) c_int {
    const c: *RangeCursor = @ptrCast(@alignCast(cur.?));
    c.current += 1;
    return RQL_OK;
}

fn rangeEof(cur: ?*RqlVtabCursor) callconv(.C) c_int {
    const c: *RangeCursor = @ptrCast(@alignCast(cur.?));
    return if (c.current >= c.stop) 1 else 0;
}

fn rangeColumn(cur: ?*RqlVtabCursor, ctx: ?*RqlContext, i: c_int) callconv(.C) c_int {
    _ = i;
    const c: *RangeCursor = @ptrCast(@alignCast(cur.?));
    rql.result_int64(ctx, c.current);
    return RQL_OK;
}

fn rangeRowid(cur: ?*RqlVtabCursor, out: ?*rql_int64) callconv(.C) c_int {
    const c: *RangeCursor = @ptrCast(@alignCast(cur.?));
    out.?.* = c.current;
    return RQL_OK;
}

fn rangeDisconnect(vtab: ?*RqlVtab) callconv(.C) c_int {
    const v: *RangeVtab = @ptrCast(@alignCast(vtab.?));
    std.heap.c_allocator.destroy(v);
    return RQL_OK;
}

var range_module = RqlModule{
    .i_version = 1,
    .x_create = rangeCreate,
    .x_connect = rangeCreate,
    .x_best_index = rangeBestIndex,
    .x_disconnect = rangeDisconnect,
    .x_destroy = rangeDisconnect,
    .x_open = rangeOpen,
    .x_close = rangeClose,
    .x_filter = rangeFilter,
    .x_next = rangeNext,
    .x_eof = rangeEof,
    .x_column = rangeColumn,
    .x_rowid = rangeRowid,
    .x_update = null,
};

// --------------------------------------------------------------- entry

export fn rustqlite_extension_init(api: ?*const RqlApi, db: ?*RqlDb, err: ?*?[*:0]CChar) c_int {
    rql = api orelse return RQL_MISUSE;
    if (rql.version < 1) return RQL_ERROR;
    if (err != null) err.?.* = null;

    if (rql.create_function(db, @ptrCast("rot13"), 1, 0, null, rot13Func, null, null) != RQL_OK)
        return RQL_ERROR;
    if (rql.create_function(db, @ptrCast("zcount"), 1, 0, null, null, zcountStep, zcountFinal) != RQL_OK)
        return RQL_ERROR;
    if (rql.create_collation(db, @ptrCast("ZREVERSE"), null, zreverseCollation) != RQL_OK)
        return RQL_ERROR;
    if (rql.create_module(db, @ptrCast("zrange"), &range_module, null) != RQL_OK)
        return RQL_ERROR;
    return RQL_OK;
}
