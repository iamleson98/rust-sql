//! A rustqlite dynamic extension written in Rust against the raw C ABI.
//!
//! This crate builds to `librustext.so` WITHOUT linking the engine — it
//! talks only through the `rql_api` table, exactly like the C/C++/Zig
//! examples. That proves the ABI is language-agnostic and stable: the
//! engine can load this .so without any Rust type shared at the boundary.
//!
//! (Rust extensions embedded directly in the application can instead use
//! the safe `rustqlite::plugin` traits and `Database::create_function` —
//! no dynamic loading involved. This crate is for the runtime-loadable
//! case, e.g. shipping a plugin binary for an app you don't compile.)
//!
//! Registers:
//!   - revsum(nums...)  : scalar — sum of the arguments, reversed digits
//!   - product(x)       : aggregate — product of non-NULL values
//!   - "mirror" vtab    : SELECT * FROM mirror WHERE n = ? (echoes rows)
//!
//! Build: cargo build --release  →  target/release/librustext.so

// ---------------------------------------------------------------------------
// ABI surface (matches include/rustqlite_ext.h — kept in sync manually
// because this crate deliberately does NOT depend on the engine crate).
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct Api {
    version: i32,
    result_int64: unsafe extern "C" fn(*mut Ctx, i64),
    result_double: unsafe extern "C" fn(*mut Ctx, f64),
    result_text: unsafe extern "C" fn(*mut Ctx, *const i8, i32),
    result_blob: unsafe extern "C" fn(*mut Ctx, *const u8, i32),
    result_null: unsafe extern "C" fn(*mut Ctx),
    result_error: unsafe extern "C" fn(*mut Ctx, *const i8, i32),
    value_type: unsafe extern "C" fn(*mut Val) -> i32,
    value_int64: unsafe extern "C" fn(*mut Val) -> i64,
    value_double: unsafe extern "C" fn(*mut Val) -> f64,
    value_text: unsafe extern "C" fn(*mut Val, *mut i32) -> *const i8,
    value_blob: unsafe extern "C" fn(*mut Val, *mut i32) -> *const u8,
    value_bytes: unsafe extern "C" fn(*mut Val) -> i32,
    aggregate_context: unsafe extern "C" fn(*mut Ctx, i32) -> *mut u8,
    create_function: unsafe extern "C" fn(
        *mut Db,
        *const i8,
        i32,
        i32,
        *mut u8,
        Option<unsafe extern "C" fn(*mut Ctx, i32, *mut *mut Val)>,
        Option<unsafe extern "C" fn(*mut Ctx, i32, *mut *mut Val)>,
        Option<unsafe extern "C" fn(*mut Ctx)>,
    ) -> i32,
    create_collation: unsafe extern "C" fn(
        *mut Db,
        *const i8,
        *mut u8,
        unsafe extern "C" fn(*mut u8, i32, *const u8, i32, *const u8) -> i32,
    ) -> i32,
    create_module: unsafe extern "C" fn(*mut Db, *const i8, *const Module, *mut u8) -> i32,
    declare_vtab: unsafe extern "C" fn(*mut Db, *const i8) -> i32,
    exec: unsafe extern "C" fn(*mut Db, *const i8) -> i32,
    errmsg: unsafe extern "C" fn(*mut Db) -> *const i8,
    malloc: unsafe extern "C" fn(usize) -> *mut u8,
    free: unsafe extern "C" fn(*mut u8),
    engine_version: unsafe extern "C" fn() -> *const i8,
}

#[repr(C)]
pub struct Db {
    _p: [u8; 0],
}
#[repr(C)]
pub struct Val {
    _p: [u8; 0],
}
#[repr(C)]
pub struct Ctx {
    _p: [u8; 0],
}
#[repr(C)]
pub struct Vtab {
    p_module: *const Module,
    p_aux: *mut u8,
    z_err_msg: *mut i8,
}
#[repr(C)]
pub struct VtabCursor {
    p_vtab: *mut Vtab,
}
#[repr(C)]
pub struct IndexConstraint {
    column: i32,
    op: i32,
    usable: u8,
    _pad: [u8; 3],
}
#[repr(C)]
pub struct IndexInfo {
    n_constraint: i32,
    a_constraint: *const IndexConstraint,
    idx_num: i32,
    idx_str: *mut i8,
    a_constraint_usage: *mut u8,
    estimated_cost: f64,
    estimated_rows: i64,
}

type CreateFn = unsafe extern "C" fn(
    *mut Db,
    *mut u8,
    i32,
    *const *const i8,
    *mut *mut Vtab,
    *mut *mut i8,
) -> i32;

#[repr(C)]
pub struct Module {
    i_version: i32,
    x_create: Option<CreateFn>,
    x_connect: Option<CreateFn>,
    x_best_index: Option<unsafe extern "C" fn(*mut Vtab, *mut IndexInfo) -> i32>,
    x_disconnect: Option<unsafe extern "C" fn(*mut Vtab) -> i32>,
    x_destroy: Option<unsafe extern "C" fn(*mut Vtab) -> i32>,
    x_open: Option<unsafe extern "C" fn(*mut Vtab, *mut *mut VtabCursor) -> i32>,
    x_close: Option<unsafe extern "C" fn(*mut VtabCursor) -> i32>,
    x_filter: Option<unsafe extern "C" fn(*mut VtabCursor, i32, *const i8, i32, *mut *mut Val) -> i32>,
    x_next: Option<unsafe extern "C" fn(*mut VtabCursor) -> i32>,
    x_eof: Option<unsafe extern "C" fn(*mut VtabCursor) -> i32>,
    x_column: Option<unsafe extern "C" fn(*mut VtabCursor, *mut Ctx, i32) -> i32>,
    x_rowid: Option<unsafe extern "C" fn(*mut VtabCursor, *mut i64) -> i32>,
    x_update: Option<unsafe extern "C" fn(*mut Vtab, i32, *mut *mut Val, *mut i64) -> i32>,
}

static mut API: Option<&'static Api> = None;

fn api_impl() -> &'static Api {
    unsafe { API.expect("extension not initialized") }
}

fn api() -> &'static Api {
    api_impl()
}

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap()
}

fn cstr_ptr(s: &std::ffi::CString) -> *const i8 {
    s.as_ptr()
}

// ---------------------------------------------------------------------------
// revsum: sum args, then reverse the decimal digits.
// ---------------------------------------------------------------------------

unsafe extern "C" fn revsum(ctx: *mut Ctx, argc: i32, argv: *mut *mut Val) {
    let mut sum: i64 = 0;
    for i in 0..argc.max(0) as usize {
        let v = *argv.add(i);
        if v.is_null() || (api().value_type)(v) == 5 {
            continue;
        }
        sum = sum.wrapping_add((api().value_int64)(v));
    }
    let rev: i64 = sum
        .abs()
        .to_string()
        .chars()
        .rev()
        .collect::<String>()
        .parse()
        .unwrap_or(0);
    let out = if sum < 0 { -rev } else { rev };
    (api().result_int64)(ctx, out);
}

// ---------------------------------------------------------------------------
// product: aggregate with plugin-managed state.
// ---------------------------------------------------------------------------

#[repr(C)]
struct ProductState {
    acc: f64,
    seen: i64,
}

unsafe extern "C" fn product_step(ctx: *mut Ctx, argc: i32, argv: *mut *mut Val) {
    let st = (api().aggregate_context)(ctx, std::mem::size_of::<ProductState>() as i32)
        as *mut ProductState;
    if st.is_null() {
        return;
    }
    if argc >= 1 {
        let v = *argv.add(0);
        if !v.is_null() && (api().value_type)(v) != 5 {
            let x = (api().value_double)(v);
            // aggregate_context zero-initializes, so the multiplicative
            // identity must be established on the first value.
            if (*st).seen == 0 {
                (*st).acc = x;
            } else {
                (*st).acc *= x;
            }
            (*st).seen += 1;
        }
    }
}

unsafe extern "C" fn product_final(ctx: *mut Ctx) {
    let st = (api().aggregate_context)(ctx, 0) as *mut ProductState;
    if st.is_null() || (*st).seen == 0 {
        (api().result_null)(ctx);
    } else {
        (api().result_double)(ctx, (*st).acc);
    }
}

// ---------------------------------------------------------------------------
// "mirror" vtab: echoes `count` rows (n, text) with an equality pushdown.
// ---------------------------------------------------------------------------

#[repr(C)]
struct MirrorVtab {
    base: Vtab,
    count: i64,
}

#[repr(C)]
struct MirrorCursor {
    base: VtabCursor,
    current: i64,
    end: i64,
}

unsafe extern "C" fn mirror_create(
    db: *mut Db,
    _aux: *mut u8,
    argc: i32,
    argv: *const *const i8,
    pp: *mut *mut Vtab,
    _err: *mut *mut i8,
) -> i32 {
    let count = if argc >= 4 {
        let arg = *argv.add(3);
        if arg.is_null() {
            5
        } else {
            std::ffi::CStr::from_ptr(arg)
                .to_string_lossy()
                .parse()
                .unwrap_or(5)
        }
    } else {
        5
    };
    let sql = b"CREATE TABLE x(n INTEGER, label TEXT)\0";
    if (api().declare_vtab)(db, sql.as_ptr() as *const i8) != 0 {
        return 1;
    }
    let v = Box::into_raw(Box::new(MirrorVtab {
        base: Vtab {
            p_module: std::ptr::null(),
            p_aux: std::ptr::null_mut(),
            z_err_msg: std::ptr::null_mut(),
        },
        count,
    }));
    *pp = v as *mut Vtab;
    0
}

unsafe extern "C" fn mirror_disconnect(v: *mut Vtab) -> i32 {
    drop(Box::from_raw(v as *mut MirrorVtab));
    0
}

unsafe extern "C" fn mirror_best_index(_v: *mut Vtab, info: *mut IndexInfo) -> i32 {
    for i in 0..(*info).n_constraint.max(0) as usize {
        let c = &*(*info).a_constraint.add(i);
        if c.usable == 1 && c.column == 0 && c.op == 2 {
            *(*info).a_constraint_usage.add(i) = 1;
        }
    }
    (*info).idx_num = 1;
    (*info).estimated_rows = 10;
    (*info).estimated_cost = 10.0;
    0
}

unsafe extern "C" fn mirror_open(v: *mut Vtab, pp: *mut *mut VtabCursor) -> i32 {
    let c = Box::into_raw(Box::new(MirrorCursor {
        base: VtabCursor { p_vtab: v },
        current: 0,
        end: 5,
    }));
    *pp = c as *mut VtabCursor;
    0
}

unsafe extern "C" fn mirror_close(c: *mut VtabCursor) -> i32 {
    drop(Box::from_raw(c as *mut MirrorCursor));
    0
}

unsafe extern "C" fn mirror_filter(
    c: *mut VtabCursor,
    idx: i32,
    _idx_str: *const i8,
    argc: i32,
    argv: *mut *mut Val,
) -> i32 {
    let mc = &mut *(c as *mut MirrorCursor);
    let v = &mut *(mc.base.p_vtab as *mut MirrorVtab);
    mc.current = 0;
    mc.end = v.count;
    if idx == 1 && argc >= 1 {
        let arg = *argv.add(0);
        if !arg.is_null() {
            // EQ pushdown: yield exactly the matching row (when in range).
            mc.current = (api().value_int64)(arg);
            mc.end = mc.current + 1;
        }
    }
    0
}

unsafe extern "C" fn mirror_next(c: *mut VtabCursor) -> i32 {
    let mc = &mut *(c as *mut MirrorCursor);
    mc.current += 1;
    0
}

unsafe extern "C" fn mirror_eof(c: *mut VtabCursor) -> i32 {
    let mc = &*(c as *const MirrorCursor);
    if mc.current >= mc.end {
        1
    } else {
        0
    }
}

unsafe extern "C" fn mirror_column(c: *mut VtabCursor, ctx: *mut Ctx, i: i32) -> i32 {
    let mc = &*(c as *const MirrorCursor);
    if i == 0 {
        (api().result_int64)(ctx, mc.current);
    } else {
        let label = cstr(&format!("mirror-{}", mc.current));
        (api().result_text)(ctx, label.as_ptr(), -1);
        std::mem::forget(label); // the engine copies during result_text
    }
    0
}

unsafe extern "C" fn mirror_rowid(c: *mut VtabCursor, out: *mut i64) -> i32 {
    let mc = &*(c as *const MirrorCursor);
    *out = mc.current;
    0
}

static MODULE: Module = Module {
    i_version: 1,
    x_create: Some(mirror_create),
    x_connect: Some(mirror_create),
    x_best_index: Some(mirror_best_index),
    x_disconnect: Some(mirror_disconnect),
    x_destroy: Some(mirror_disconnect),
    x_open: Some(mirror_open),
    x_close: Some(mirror_close),
    x_filter: Some(mirror_filter),
    x_next: Some(mirror_next),
    x_eof: Some(mirror_eof),
    x_column: Some(mirror_column),
    x_rowid: Some(mirror_rowid),
    x_update: None,
};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn rustqlite_extension_init(
    api: *const Api,
    db: *mut Db,
    err: *mut *mut i8,
) -> i32 {
    if api.is_null() {
        return 21;
    }
    API = Some(std::mem::transmute::<*const Api, &'static Api>(api));
    let a = api_impl();
    if !err.is_null() {
        *err = std::ptr::null_mut();
    }
    let revsum_name = cstr("revsum");
    if (a.create_function)(db, cstr_ptr(&revsum_name), -1, 0, std::ptr::null_mut(), Some(revsum), None, None) != 0 {
        return 1;
    }
    let product_name = cstr("product");
    if (a.create_function)(db, cstr_ptr(&product_name), 1, 0, std::ptr::null_mut(), None, Some(product_step), Some(product_final)) != 0 {
        return 1;
    }
    let module_name = cstr("mirror");
    if (a.create_module)(db, cstr_ptr(&module_name), &MODULE, std::ptr::null_mut()) != 0 {
        return 1;
    }
    0
}
