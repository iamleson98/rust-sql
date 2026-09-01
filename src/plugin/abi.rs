//! C ABI for dynamic extensions: the `rql_api` function table, trampoline
//! adapters, and `dlopen`-based loading.
//!
//! An extension is any shared library (`.so` / `.dylib` / `.dll`) exporting
//!
//! ```c
//! int rustqlite_extension_init(const rql_api *api, rql_db *db, char **err);
//! ```
//!
//! and calling `api->create_function` / `create_collation` / `create_module`
//! to register itself. The same header (`include/rustqlite_ext.h`) works
//! from C, C++, Zig, and Rust (compiled as `cdylib`).
//!
//! The trampolines adapt C function pointers onto the safe Rust traits
//! ([`crate::plugin::ScalarFunction`] etc.), so registered extensions flow
//! through exactly the same dispatch as native Rust plugins.

use crate::error::{Error, Result};
use crate::types::Value;
use std::ffi::{c_char, c_int, c_void, CStr, CString};

use crate::plugin::{AggregateFunction, AggCtx, AggState, Collation, FnCtx, ScalarFunction};

/// Value type codes (SQLite-compatible numbering).
pub const RQL_INTEGER: c_int = 1;
pub const RQL_FLOAT: c_int = 2;
pub const RQL_TEXT: c_int = 3;
pub const RQL_BLOB: c_int = 4;
pub const RQL_NULL: c_int = 5;

/// Status codes (SQLite-compatible numbering for familiarity).
pub const RQL_OK: c_int = 0;
pub const RQL_ERROR: c_int = 1;
pub const RQL_NOMEM: c_int = 2;
pub const RQL_MISUSE: c_int = 21;

// ---------------------------------------------------------------------------
// Opaque handle types
// ---------------------------------------------------------------------------

/// Opaque SQL value handle (a `*mut` to a boxed Value).
#[repr(C)]
pub struct RqlValue {
    _private: [u8; 0],
}

/// Function-call context: result slot + error slot + aggregate state.
#[repr(C)]
pub struct RqlContext {
    _private: [u8; 0],
}

/// Connection handle (see `crate::ffi`).
#[repr(C)]
pub struct RqlDb {
    _private: [u8; 0],
}

// ---------------------------------------------------------------------------
// The API table
// ---------------------------------------------------------------------------

/// The function table handed to `rustqlite_extension_init` — the
/// rustqlite analogue of `sqlite3_api_routines`.
///
/// All function pointers are valid for the lifetime of the process (they
/// are static trampolines), so extensions may keep the table.
#[repr(C)]
pub struct RqlApi {
    pub version: c_int,
    // --- results (xFunc / xFinal) ---
    pub result_int64: unsafe extern "C" fn(ctx: *mut RqlContext, v: i64),
    pub result_double: unsafe extern "C" fn(ctx: *mut RqlContext, v: f64),
    /// Copies `len` bytes (a negative length uses strlen).
    pub result_text: unsafe extern "C" fn(ctx: *mut RqlContext, s: *const c_char, len: c_int),
    pub result_blob: unsafe extern "C" fn(ctx: *mut RqlContext, data: *const c_void, len: c_int),
    pub result_null: unsafe extern "C" fn(ctx: *mut RqlContext),
    /// Copies the message; the statement fails with it afterwards.
    pub result_error: unsafe extern "C" fn(ctx: *mut RqlContext, msg: *const c_char, len: c_int),
    // --- value access ---
    pub value_type: unsafe extern "C" fn(v: *mut RqlValue) -> c_int,
    pub value_int64: unsafe extern "C" fn(v: *mut RqlValue) -> i64,
    pub value_double: unsafe extern "C" fn(v: *mut RqlValue) -> f64,
    /// Returns a NUL-terminated pointer valid until the statement is
    /// finalized / re-stepped; `plen` (optional) receives the byte length.
    pub value_text: unsafe extern "C" fn(v: *mut RqlValue, plen: *mut c_int) -> *const c_char,
    pub value_blob: unsafe extern "C" fn(v: *mut RqlValue, plen: *mut c_int) -> *const c_void,
    pub value_bytes: unsafe extern "C" fn(v: *mut RqlValue) -> c_int,
    // --- aggregates ---
    /// Returns per-group, zero-initialized state memory (allocated once,
    /// freed after xFinal). NULL on allocation failure.
    pub aggregate_context: unsafe extern "C" fn(ctx: *mut RqlContext, n_bytes: c_int) -> *mut c_void,
    // --- registration ---
    pub create_function: unsafe extern "C" fn(
        db: *mut RqlDb,
        name: *const c_char,
        n_arg: c_int,
        e_text_rep: c_int,
        p_app: *mut c_void,
        x_func: Option<unsafe extern "C" fn(ctx: *mut RqlContext, argc: c_int, argv: *mut *mut RqlValue)>,
        x_step: Option<unsafe extern "C" fn(ctx: *mut RqlContext, argc: c_int, argv: *mut *mut RqlValue)>,
        x_final: Option<unsafe extern "C" fn(ctx: *mut RqlContext)>,
    ) -> c_int,
    pub create_collation: unsafe extern "C" fn(
        db: *mut RqlDb,
        name: *const c_char,
        p_app: *mut c_void,
        x_compare: unsafe extern "C" fn(
            p_app: *mut c_void,
            len1: c_int,
            ptr1: *const c_void,
            len2: c_int,
            ptr2: *const c_void,
        ) -> c_int,
    ) -> c_int,
    /// Register a virtual-table module (C vtab protocol).
    pub create_module: unsafe extern "C" fn(
        db: *mut RqlDb,
        name: *const c_char,
        module: *const RqlModule,
        p_aux: *mut c_void,
    ) -> c_int,
    /// Declare a vtab's columns from xCreate/xConnect (the module passes
    /// a `CREATE TABLE x(a TYPE, ...)` statement; the engine parses it).
    pub declare_vtab: unsafe extern "C" fn(db: *mut RqlDb, sql: *const c_char) -> c_int,
    // --- misc ---
    pub exec: unsafe extern "C" fn(db: *mut RqlDb, sql: *const c_char) -> c_int,
    pub errmsg: unsafe extern "C" fn(db: *mut RqlDb) -> *const c_char,
    pub malloc: unsafe extern "C" fn(n: usize) -> *mut c_void,
    pub free: unsafe extern "C" fn(p: *mut c_void),
    /// The engine's version string.
    pub engine_version: unsafe extern "C" fn() -> *const c_char,
}

// ---------------------------------------------------------------------------
// Virtual table C protocol
// ---------------------------------------------------------------------------

/// Virtual-table module descriptor (C side). Registered with
/// `api->create_module`; must stay valid for the connection's lifetime.
#[repr(C)]
pub struct RqlModule {
    pub i_version: c_int,
    /// `argv`: module name, db name, table name, then user args (SQLite
    /// protocol). Declare columns via `api->declare_vtab`.
    pub x_create: Option<
        unsafe extern "C" fn(db: *mut RqlDb, p_aux: *mut c_void, argc: c_int, argv: *const *const c_char, pp_vtab: *mut *mut RqlVtab, perr: *mut *mut c_char) -> c_int,
    >,
    pub x_connect: Option<
        unsafe extern "C" fn(db: *mut RqlDb, p_aux: *mut c_void, argc: c_int, argv: *const *const c_char, pp_vtab: *mut *mut RqlVtab, perr: *mut *mut c_char) -> c_int,
    >,
    pub x_best_index: Option<unsafe extern "C" fn(vtab: *mut RqlVtab, info: *mut RqlIndexInfo) -> c_int>,
    pub x_disconnect: Option<unsafe extern "C" fn(vtab: *mut RqlVtab) -> c_int>,
    pub x_destroy: Option<unsafe extern "C" fn(vtab: *mut RqlVtab) -> c_int>,
    pub x_open: Option<unsafe extern "C" fn(vtab: *mut RqlVtab, pp_cursor: *mut *mut RqlVtabCursor) -> c_int>,
    pub x_close: Option<unsafe extern "C" fn(cursor: *mut RqlVtabCursor) -> c_int>,
    pub x_filter: Option<
        unsafe extern "C" fn(cursor: *mut RqlVtabCursor, idx_num: c_int, idx_str: *const c_char, argc: c_int, argv: *mut *mut RqlValue) -> c_int,
    >,
    pub x_next: Option<unsafe extern "C" fn(cursor: *mut RqlVtabCursor) -> c_int>,
    /// Returns 1 at end-of-scan, 0 otherwise.
    pub x_eof: Option<unsafe extern "C" fn(cursor: *mut RqlVtabCursor) -> c_int>,
    /// Reads column `i` into the context: call `api->result_int64(ctx, v)`
    /// / `result_text` / `result_null` from the module (SQLite's
    /// xColumn + sqlite3_result_* model).
    pub x_column: Option<unsafe extern "C" fn(cursor: *mut RqlVtabCursor, ctx: *mut RqlContext, i: c_int) -> c_int>,
    pub x_rowid: Option<unsafe extern "C" fn(cursor: *mut RqlVtabCursor, p_rowid: *mut i64) -> c_int>,
    /// xUpdate protocol (SQLite): argv[0] = old rowid (NULL for insert),
    /// argv[1..n_cols] = new values (NULL = unchanged); empty argv
    /// (argc == 1 with a NULL argv[0] and no columns) = delete. Modules
    /// not providing x_update are read-only.
    pub x_update: Option<
        unsafe extern "C" fn(vtab: *mut RqlVtab, argc: c_int, argv: *mut *mut RqlValue, p_rowid: *mut i64) -> c_int,
    >,
}

/// vtab instance header (the C plugin's struct starts with this).
#[repr(C)]
pub struct RqlVtab {
    pub p_module: *const RqlModule,
    /// The engine fills this with the plugin's `p_aux` from create_module.
    pub p_aux: *mut c_void,
    /// Error message slot: the engine reads (and frees) it after calls
    /// that return a non-OK status.
    pub z_err_msg: *mut c_char,
}

/// vtab cursor header (the C plugin's cursor struct starts with this).
#[repr(C)]
pub struct RqlVtabCursor {
    pub p_vtab: *mut RqlVtab,
}

/// One constraint passed to xBestIndex.
#[repr(C)]
pub struct RqlIndexConstraint {
    /// Column index, or -1 for the rowid.
    pub column: c_int,
    /// RQL_INDEX_EQ=2, GT=4, LE=8, LT=16, GE=32, LIKE=66, GLOB=74
    /// (SQLite's values).
    pub op: c_int,
    pub usable: u8,
    _pad: [u8; 3],
}

/// xBestIndex input/output block.
#[repr(C)]
pub struct RqlIndexInfo {
    pub n_constraint: c_int,
    pub a_constraint: *const RqlIndexConstraint,
    /// OUT: opaque strategy id.
    pub idx_num: c_int,
    /// OUT: strategy string (allocated with api->malloc; the engine frees
    /// it with api->free after the scan).
    pub idx_str: *mut c_char,
    /// OUT: one flag per constraint: 1 = the module handles it.
    pub a_constraint_usage: *mut u8,
    /// OUT: estimated cost (lower = better).
    pub estimated_cost: f64,
    /// OUT: estimated rows.
    pub estimated_rows: i64,
}

// ---------------------------------------------------------------------------
// Trampoline state (call-scoped)
// ---------------------------------------------------------------------------

/// Internal representation of `RqlContext`.
#[allow(dead_code)]
pub(crate) struct CallCtx {
    pub out: Option<Value>,
    pub err: Option<String>,
    /// Aggregate state pointer block (plugin-managed memory).
    pub agg_mem: Option<*mut u8>,
    pub agg_len: usize,
    /// The p_app pointer registered with the function.
    pub app: *mut c_void,
    /// Text pointers handed out via value_text during this call (freed
    /// when the call returns).
    pub leaked: Vec<*mut c_void>,
}

/// Boxed value handle (what `*mut RqlValue` actually points to).
#[allow(dead_code)]
pub(crate) type ValueHandle = Box<Value>;

/// Convert a `*mut RqlValue` handle into a reference.
unsafe fn value_ref(v: *mut RqlValue) -> Option<&'static Value> {
    if v.is_null() {
        return None;
    }
    (v as *const Value).as_ref()
}

unsafe fn cstr_or_len(s: *const c_char, len: c_int) -> Vec<u8> {
    if s.is_null() {
        return Vec::new();
    }
    if len < 0 {
        CStr::from_ptr(s).to_bytes().to_vec()
    } else {
        std::slice::from_raw_parts(s as *const u8, len as usize).to_vec()
    }
}

// ---------------------------------------------------------------------------
// Static trampolines (the api table's implementations)
// ---------------------------------------------------------------------------

// The current call context is thread-local (a statement executes on one
// thread; SQLite has the same constraint for xFunc).
thread_local! {
    static CALL: std::cell::RefCell<Option<*mut CallCtx>> = const { std::cell::RefCell::new(None) };
}

#[allow(dead_code)]
pub(crate) fn with_call_ctx<R>(f: impl FnOnce(&mut CallCtx) -> R) -> Option<R> {
    CALL.with(|c| c.borrow().map(|p| unsafe { f(&mut *p) }))
}

pub(crate) fn set_call_ctx(p: *mut CallCtx) {
    CALL.with(|c| *c.borrow_mut() = Some(p));
}

pub(crate) fn clear_call_ctx() {
    CALL.with(|c| *c.borrow_mut() = None);
}

// --- results ---

unsafe extern "C" fn tramp_result_int64(ctx: *mut RqlContext, v: i64) {
    with_ctx_ptr(ctx, |c| c.out = Some(Value::Integer(v)));
}

unsafe extern "C" fn tramp_result_double(ctx: *mut RqlContext, v: f64) {
    with_ctx_ptr(ctx, |c| c.out = Some(Value::Real(v)));
}

unsafe extern "C" fn tramp_result_text(ctx: *mut RqlContext, s: *const c_char, len: c_int) {
    let bytes = cstr_or_len(s, len);
    with_ctx_ptr(ctx, |c| c.out = Some(Value::Text(String::from_utf8_lossy(&bytes).into_owned().into())));
}

unsafe extern "C" fn tramp_result_blob(ctx: *mut RqlContext, data: *const c_void, len: c_int) {
    let bytes = if data.is_null() || len <= 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(data as *const u8, len as usize).to_vec()
    };
    with_ctx_ptr(ctx, |c| c.out = Some(Value::Blob(bytes)));
}

unsafe extern "C" fn tramp_result_null(ctx: *mut RqlContext) {
    with_ctx_ptr(ctx, |c| c.out = Some(Value::Null));
}

unsafe extern "C" fn tramp_result_error(ctx: *mut RqlContext, msg: *const c_char, len: c_int) {
    let bytes = cstr_or_len(msg, len);
    with_ctx_ptr(ctx, |c| {
        c.err = Some(String::from_utf8_lossy(&bytes).into_owned());
    });
}

fn with_ctx_ptr<R>(ctx: *mut RqlContext, f: impl FnOnce(&mut CallCtx) -> R) -> Option<R> {
    if ctx.is_null() {
        return None;
    }
    // The context pointer IS a CallCtx pointer.
    let p = ctx as *mut CallCtx;
    unsafe { Some(f(&mut *p)) }
}

// --- value access ---

unsafe extern "C" fn tramp_value_type(v: *mut RqlValue) -> c_int {
    match value_ref(v) {
        Some(Value::Null) | None => RQL_NULL,
        Some(Value::Integer(_)) => RQL_INTEGER,
        Some(Value::Real(_)) => RQL_FLOAT,
        Some(Value::Text(_)) => RQL_TEXT,
        Some(Value::Blob(_)) => RQL_BLOB,
    }
}

unsafe extern "C" fn tramp_value_int64(v: *mut RqlValue) -> i64 {
    value_ref(v).map(|val| val.as_integer()).unwrap_or(0)
}

unsafe extern "C" fn tramp_value_double(v: *mut RqlValue) -> f64 {
    value_ref(v).map(|val| val.as_real()).unwrap_or(0.0)
}

unsafe extern "C" fn tramp_value_text(v: *mut RqlValue, plen: *mut c_int) -> *const c_char {
    match value_ref(v) {
        Some(Value::Text(t)) => {
            let cs = match CString::new(t.as_str().as_bytes()) {
                Ok(c) => c,
                Err(_) => CString::new("").unwrap(),
            };
            let n = cs.as_bytes().len();
            let ptr = cs.as_ptr();
            if !plen.is_null() {
                *plen = n as c_int;
            }
            // Leak into the call context scratch (freed after the call).
            std::mem::forget(cs);
            // Ownership tracking: store in a per-call leak list to free at
            // statement end. For simplicity, transfer into the CURRENT
            // call context when available; otherwise leak intentionally
            // bounded by the call count (contexts always exist for
            // plugin calls).
            CALL.with(|c| {
                let b = c.borrow_mut();
                if let Some(p) = *b {
                    // SAFETY: extend the scratch list on CallCtx.
                    let ctx = unsafe { &mut *p };
                    ctx.leaked.push(ptr as *mut c_void);
                } else {
                    // Detached call (ffi column access): track globally.
                    LEAKED.with(|l| l.borrow_mut().push(ptr as *mut c_void));
                }
            });
            ptr
        }
        _ => {
            if !plen.is_null() {
                *plen = 0;
            }
            std::ptr::null()
        }
    }
}

thread_local! {
    /// Text pointers handed out outside a plugin call (ffi column_text).
    static LEAKED: std::cell::RefCell<Vec<*mut c_void>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Free all detached text pointers (called on reset/finalize).
#[allow(dead_code)]
pub(crate) fn free_detached_text() {
    LEAKED.with(|l| {
        for p in l.borrow_mut().drain(..) {
            unsafe { drop(CString::from_raw(p as *mut c_char)) };
        }
    });
}

unsafe extern "C" fn tramp_value_blob(v: *mut RqlValue, plen: *mut c_int) -> *const c_void {
    match value_ref(v) {
        Some(Value::Blob(b)) => {
            if !plen.is_null() {
                *plen = b.len() as c_int;
            }
            b.as_ptr() as *const c_void
        }
        _ => {
            if !plen.is_null() {
                *plen = 0;
            }
            std::ptr::null()
        }
    }
}

unsafe extern "C" fn tramp_value_bytes(v: *mut RqlValue) -> c_int {
    match value_ref(v) {
        Some(Value::Text(t)) => t.as_str().as_bytes().len() as c_int,
        Some(Value::Blob(b)) => b.len() as c_int,
        _ => 0,
    }
}

unsafe extern "C" fn tramp_aggregate_context(ctx: *mut RqlContext, n_bytes: c_int) -> *mut c_void {
    with_ctx_ptr(ctx, |c| {
        match c.agg_mem {
            // Existing block: return it regardless of n_bytes (SQLite's
            // xFinal calls aggregate_context(ctx, 0) to peek).
            Some(p) => p as *mut c_void,
            None => {
                if n_bytes <= 0 {
                    return std::ptr::null_mut();
                }
                let p = unsafe { std::alloc::alloc_zeroed(std::alloc::Layout::from_size_align_unchecked(n_bytes as usize, 8)) };
                c.agg_mem = Some(p);
                c.agg_len = n_bytes as usize;
                p as *mut c_void
            }
        }
    })
    .unwrap_or(std::ptr::null_mut())
}

unsafe extern "C" fn tramp_malloc(n: usize) -> *mut c_void {
    // 16-byte header before the returned pointer stores the allocation
    // size; tramp_free recovers it.
    let cap = n + 16;
    let layout = std::alloc::Layout::from_size_align_unchecked(cap, 16);
    let base = unsafe { std::alloc::alloc(layout) };
    if base.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        *(base as *mut usize) = cap;
        base.add(16) as *mut c_void
    }
}

unsafe extern "C" fn tramp_free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    unsafe {
        let base = (p as *mut u8).sub(16);
        let cap = *(base as *const usize);
        let layout = std::alloc::Layout::from_size_align_unchecked(cap, 16);
        std::alloc::dealloc(base, layout);
    }
}

unsafe extern "C" fn tramp_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

// --- registration trampolines (forwarded into crate::ffi) ---

unsafe extern "C" fn tramp_create_function(
    db: *mut RqlDb,
    name: *const c_char,
    n_arg: c_int,
    _e_text_rep: c_int,
    p_app: *mut c_void,
    x_func: Option<unsafe extern "C" fn(*mut RqlContext, c_int, *mut *mut RqlValue)>,
    x_step: Option<unsafe extern "C" fn(*mut RqlContext, c_int, *mut *mut RqlValue)>,
    x_final: Option<unsafe extern "C" fn(*mut RqlContext)>,
) -> c_int {
    let name = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return RQL_MISUSE,
    };
    let db_ref = match crate::ffi::db_from_handle(db) {
        Some(d) => d,
        None => return RQL_MISUSE,
    };
    let res = if let Some(xf) = x_func {
        crate::ffi::register_c_scalar(db_ref, &name, n_arg, p_app, xf)
    } else if let (Some(xs), Some(xfin)) = (x_step, x_final) {
        crate::ffi::register_c_aggregate(db_ref, &name, n_arg, p_app, xs, xfin)
    } else {
        Err(Error::semantic("create_function requires xFunc or xStep+xFinal"))
    };
    match res {
        Ok(()) => RQL_OK,
        Err(_) => RQL_ERROR,
    }
}

unsafe extern "C" fn tramp_create_collation(
    db: *mut RqlDb,
    name: *const c_char,
    p_app: *mut c_void,
    x_compare: unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
) -> c_int {
    let name = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return RQL_MISUSE,
    };
    let db_ref = match crate::ffi::db_from_handle(db) {
        Some(d) => d,
        None => return RQL_MISUSE,
    };
    match crate::ffi::register_c_collation(db_ref, &name, p_app, x_compare) {
        Ok(()) => RQL_OK,
        Err(_) => RQL_ERROR,
    }
}

unsafe extern "C" fn tramp_create_module(
    db: *mut RqlDb,
    name: *const c_char,
    module: *const RqlModule,
    p_aux: *mut c_void,
) -> c_int {
    let name = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return RQL_MISUSE,
    };
    let db_ref = match crate::ffi::db_from_handle(db) {
        Some(d) => d,
        None => return RQL_MISUSE,
    };
    if module.is_null() {
        return RQL_MISUSE;
    }
    match crate::ffi::register_c_module(db_ref, &name, unsafe { &*module }, p_aux) {
        Ok(()) => RQL_OK,
        Err(_) => RQL_ERROR,
    }
}

thread_local! {
    /// Column declarations captured during xCreate/xConnect.
    pub(crate) static DECLARED_VTAB: std::cell::RefCell<Option<Vec<(String, String)>>> =
        const { std::cell::RefCell::new(None) };
}

unsafe extern "C" fn tramp_declare_vtab(_db: *mut RqlDb, sql: *const c_char) -> c_int {
    let sql = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return RQL_MISUSE,
    };
    // Parse "CREATE TABLE x(a INT, b TEXT)" and record the columns.
    match crate::sql::parse(sql) {
        Ok(crate::sql::ast::Statement::Create(crate::sql::ast::CreateStatement::Table { columns, .. })) => {
            let cols: Vec<(String, String)> = columns
                .iter()
                .map(|c| (c.name.clone(), c.type_name.clone()))
                .collect();
            DECLARED_VTAB.with(|d| *d.borrow_mut() = Some(cols));
            RQL_OK
        }
        _ => RQL_MISUSE,
    }
}

unsafe extern "C" fn tramp_exec(db: *mut RqlDb, sql: *const c_char) -> c_int {
    let sql = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(_) => return RQL_MISUSE,
    };
    match crate::ffi::exec_on_handle(db, sql) {
        Ok(()) => RQL_OK,
        Err(_) => RQL_ERROR,
    }
}

unsafe extern "C" fn tramp_errmsg(db: *mut RqlDb) -> *const c_char {
    crate::ffi::errmsg_ptr(db)
}

/// The process-lifetime API table. Extensions keep the `rql_api*` they
/// receive in `rustqlite_extension_init` (SQLite's contract: the routines
/// pointer stays valid), so the table must be leaked ONCE — a stack local
/// would dangle after `load_extension` returns.
pub(crate) fn api_table() -> &'static RqlApi {
    static TABLE: std::sync::OnceLock<&'static RqlApi> = std::sync::OnceLock::new();
    *TABLE.get_or_init(|| Box::leak(Box::new(build_api_table())))
}

fn build_api_table() -> RqlApi {
    RqlApi {
        version: 1,
        result_int64: tramp_result_int64,
        result_double: tramp_result_double,
        result_text: tramp_result_text,
        result_blob: tramp_result_blob,
        result_null: tramp_result_null,
        result_error: tramp_result_error,
        value_type: tramp_value_type,
        value_int64: tramp_value_int64,
        value_double: tramp_value_double,
        value_text: tramp_value_text,
        value_blob: tramp_value_blob,
        value_bytes: tramp_value_bytes,
        aggregate_context: tramp_aggregate_context,
        create_function: tramp_create_function,
        create_collation: tramp_create_collation,
        create_module: tramp_create_module,
        declare_vtab: tramp_declare_vtab,
        exec: tramp_exec,
        errmsg: tramp_errmsg,
        malloc: tramp_malloc,
        free: tramp_free,
        engine_version: tramp_version,
    }
}

// ---------------------------------------------------------------------------
// C function adapters (ScalarFunction / AggregateFunction / Collation
// implemented over the C callbacks)
// ---------------------------------------------------------------------------

/// A scalar function backed by C callbacks.
///
/// SAFETY (Send+Sync): `app` and the fn pointers are used only from the
/// calling thread during a statement (the engine's statement scope);
/// extensions register from one thread and SQLite has the same contract.
pub struct CScalar {
    pub name: String,
    pub n_arg: c_int,
    pub app: *mut c_void,
    pub x_func: unsafe extern "C" fn(*mut RqlContext, c_int, *mut *mut RqlValue),
}

// SAFETY: see the struct doc — C plugin state is confined to statement
// execution on the registering thread's lifetime model.
unsafe impl Send for CScalar {}
unsafe impl Sync for CScalar {}

unsafe impl Send for CAggregate {}
unsafe impl Sync for CAggregate {}

// SAFETY: aggregate state memory is plugin-managed and only touched from
// the executing statement's thread.
unsafe impl Send for CAggState {}

unsafe impl Send for CCollation {}
unsafe impl Sync for CCollation {}

unsafe impl Send for CVtab {}
unsafe impl Sync for CVtab {}

unsafe impl Send for CCursor {}
unsafe impl Sync for CCursor {}

unsafe impl Send for CVtabModule {}
unsafe impl Sync for CVtabModule {}

impl ScalarFunction for CScalar {
    fn name(&self) -> &str {
        &self.name
    }
    fn arity(&self) -> crate::plugin::Arity {
        if self.n_arg < 0 {
            crate::plugin::Arity::Variadic
        } else {
            crate::plugin::Arity::Exact(self.n_arg as usize)
        }
    }
    fn call(&self, _ctx: &FnCtx, args: &[Value]) -> Result<Value> {
        // Box the argument values into handles.
        let handles: Vec<Box<Value>> = args.iter().cloned().map(Box::new).collect();
        let mut argv: Vec<*mut RqlValue> = handles.iter().map(|h| h.as_ref() as *const Value as *mut RqlValue).collect();
        let mut call = CallCtx {
            out: None,
            err: None,
            agg_mem: None,
            agg_len: 0,
            app: self.app,
            leaked: Vec::new(),
        };
        let call_ptr = &mut call as *mut CallCtx;
        set_call_ctx(call_ptr);
        unsafe { (self.x_func)(call_ptr as *mut RqlContext, argv.len() as c_int, argv.as_mut_ptr()) };
        clear_call_ctx();
        // Free text pointers handed out during the call.
        for p in call.leaked.drain(..) {
            unsafe { drop(CString::from_raw(p as *mut c_char)) };
        }
        drop(handles);
        if let Some(e) = call.err {
            return Err(Error::runtime(e));
        }
        Ok(call.out.unwrap_or(Value::Null))
    }
}

/// Aggregate state backed by xStep/xFinal + `aggregate_context` memory.
pub struct CAggregate {
    pub name: String,
    pub n_arg: c_int,
    pub app: *mut c_void,
    pub x_step: unsafe extern "C" fn(*mut RqlContext, c_int, *mut *mut RqlValue),
    pub x_final: unsafe extern "C" fn(*mut RqlContext),
}

impl AggregateFunction for CAggregate {
    fn name(&self) -> &str {
        &self.name
    }
    fn arity(&self) -> crate::plugin::Arity {
        if self.n_arg < 0 {
            crate::plugin::Arity::Variadic
        } else {
            crate::plugin::Arity::Exact(self.n_arg as usize)
        }
    }
    fn init(&self) -> Box<dyn AggState> {
        Box::new(CAggState {
            app: self.app,
            x_step: self.x_step,
            x_final: self.x_final,
            n_arg: self.n_arg,
            mem: None,
            mem_len: 0,
        })
    }
}

#[allow(dead_code)]
struct CAggState {
    app: *mut c_void,
    x_step: unsafe extern "C" fn(*mut RqlContext, c_int, *mut *mut RqlValue),
    x_final: unsafe extern "C" fn(*mut RqlContext),
    n_arg: c_int,
    mem: Option<*mut u8>,
    mem_len: usize,
}

impl AggState for CAggState {
    fn step(&mut self, _ctx: &AggCtx, args: &[Value]) -> Result<()> {
        let handles: Vec<Box<Value>> = args.iter().cloned().map(Box::new).collect();
        let mut argv: Vec<*mut RqlValue> =
            handles.iter().map(|h| h.as_ref() as *const Value as *mut RqlValue).collect();
        let mut call = CallCtx {
            out: None,
            err: None,
            agg_mem: self.mem,
            agg_len: self.mem_len,
            app: self.app,
            leaked: Vec::new(),
        };
        let call_ptr = &mut call as *mut CallCtx;
        set_call_ctx(call_ptr);
        unsafe { (self.x_step)(call_ptr as *mut RqlContext, argv.len() as c_int, argv.as_mut_ptr()) };
        clear_call_ctx();
        for p in call.leaked.drain(..) {
            unsafe { drop(CString::from_raw(p as *mut c_char)) };
        }
        self.mem = call.agg_mem;
        self.mem_len = call.agg_len;
        drop(handles);
        if let Some(e) = call.err {
            return Err(Error::runtime(e));
        }
        Ok(())
    }
    fn value(&self) -> Result<Value> {
        // xFinal consumes the aggregate context (SQLite frees it after).
        let mut call = CallCtx {
            out: None,
            err: None,
            agg_mem: self.mem,
            agg_len: self.mem_len,
            app: self.app,
            leaked: Vec::new(),
        };
        let call_ptr = &mut call as *mut CallCtx;
        set_call_ctx(call_ptr);
        unsafe { (self.x_final)(call_ptr as *mut RqlContext) };
        clear_call_ctx();
        for p in call.leaked.drain(..) {
            unsafe { drop(CString::from_raw(p as *mut c_char)) };
        }
        if let Some(e) = call.err {
            return Err(Error::runtime(e));
        }
        Ok(call.out.unwrap_or(Value::Null))
    }
}

impl Drop for CAggState {
    fn drop(&mut self) {
        if let Some(p) = self.mem {
            unsafe {
                let layout = std::alloc::Layout::from_size_align_unchecked(self.mem_len.max(1), 8);
                std::alloc::dealloc(p, layout);
            }
        }
    }
}

/// Collation backed by a C comparator.
pub struct CCollation {
    pub name: String,
    pub app: *mut c_void,
    pub x_compare: unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
}

impl Collation for CCollation {
    fn name(&self) -> &str {
        &self.name
    }
    fn compare(&self, a: &str, b: &str) -> std::cmp::Ordering {
        let ab = a.as_bytes();
        let bb = b.as_bytes();
        let r = unsafe {
            (self.x_compare)(
                self.app,
                ab.len() as c_int,
                ab.as_ptr() as *const c_void,
                bb.len() as c_int,
                bb.as_ptr() as *const c_void,
            )
        };
        r.cmp(&0)
    }
}

// ---------------------------------------------------------------------------
// Dynamic loading (feature "extension")
// ---------------------------------------------------------------------------

#[cfg(feature = "extension")]
pub(crate) fn load_extension(
    db: &mut crate::api::Database,
    path: &std::path::Path,
    entry: Option<&str>,
) -> Result<()> {
    use libloading::{Library, Symbol};

    unsafe {
        let library = Library::new(path)
            .map_err(|e| Error::runtime(format!("load_extension: {}", e)))?;
        let entry_name = entry.unwrap_or("rustqlite_extension_init");
        let init: Symbol<unsafe extern "C" fn(*const RqlApi, *mut RqlDb, *mut *mut c_char) -> c_int> =
            library
                .get(entry_name.as_bytes())
                .map_err(|e| Error::runtime(format!("load_extension: entry point {}: {}", entry_name, e)))?;

        // The connection handle for the extension = the raw Database
        // pointer (valid for the duration of load_extension's &mut borrow).
        let handle = crate::ffi::make_extension_handle(db);
        let mut err: *mut c_char = std::ptr::null_mut();
        let rc = init(api_table(), handle, &mut err as *mut *mut c_char);
        if rc != RQL_OK {
            let msg = if err.is_null() {
                "extension init failed".to_string()
            } else {
                let m = CStr::from_ptr(err).to_string_lossy().into_owned();
                // The engine frees the error string (SQLite contract).
                crate::plugin::abi::tramp_free(err as *mut c_void);
                m
            };
            return Err(Error::runtime(format!("load_extension: {}", msg)));
        }
        if !err.is_null() {
            crate::plugin::abi::tramp_free(err as *mut c_void);
        }
        // Keep the library loaded for the process lifetime (SQLite does
        // the same; closing would unload code backing registered plugins).
        std::mem::forget(library);
    }
    Ok(())
}

#[cfg(not(feature = "extension"))]
pub(crate) fn load_extension(
    _db: &mut crate::api::Database,
    _path: &std::path::Path,
    _entry: Option<&str>,
) -> Result<()> {
    Err(Error::Unsupported(
        "extension loading requires the `extension` cargo feature",
    ))
}

// ---------------------------------------------------------------------------
// Virtual-table adapter: Rust VirtualTableModule over the C RqlModule
// ---------------------------------------------------------------------------

/// Raw pointer wrapper marked Send+Sync (the module struct must stay valid
/// and immutable for the connection's lifetime — the extension contract).
struct ModulePtr(*const RqlModule);
unsafe impl Send for ModulePtr {}
unsafe impl Sync for ModulePtr {}

struct AuxPtr(*mut c_void);
unsafe impl Send for AuxPtr {}
unsafe impl Sync for AuxPtr {}
impl Clone for AuxPtr {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

/// A `VirtualTableModule` backed by a C `rql_module`.
pub(crate) struct CVtabModule {
    pub name: String,
    module: ModulePtr,
    aux: AuxPtr,
    /// true when created through xConnect (reopen path).
    is_connect: std::sync::atomic::AtomicBool,
}

impl CVtabModule {
    pub fn new(name: &str, module: *const RqlModule, aux: *mut c_void) -> Self {
        Self {
            name: name.to_ascii_lowercase(),
            module: ModulePtr(module),
            aux: AuxPtr(aux),
            is_connect: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn call_create_connect(
        &self,
        db: *mut crate::api::Database,
        table: &str,
        args: &[String],
    ) -> Result<(*mut RqlVtab, Vec<(String, String)>)> {
        let db = unsafe { &mut *db };
        let m = unsafe { &*self.module.0 };
        let f = if self.is_connect.load(std::sync::atomic::Ordering::Acquire) {
            m.x_connect.or(m.x_create)
        } else {
            m.x_create.or(m.x_connect)
        };
        let f = f.ok_or_else(|| Error::Unsupported("vtab module has no xCreate"))?;
        // argv: module name, db name (path), table name, then user args.
        let mut argv_owned: Vec<CString> = Vec::with_capacity(3 + args.len());
        argv_owned.push(CString::new(self.name.clone()).unwrap());
        argv_owned.push(CString::new(db.path().to_string_lossy().as_bytes().to_vec()).unwrap());
        argv_owned.push(CString::new(table).unwrap());
        for a in args {
            argv_owned.push(CString::new(a.as_bytes().to_vec()).unwrap());
        }
        let argv: Vec<*const c_char> = argv_owned.iter().map(|c| c.as_ptr()).collect();
        DECLARED_VTAB.with(|d| *d.borrow_mut() = None);
        let mut vtab: *mut RqlVtab = std::ptr::null_mut();
        let mut err: *mut c_char = std::ptr::null_mut();
        let handle = crate::ffi::make_extension_handle(db);
        let rc = unsafe {
            f(
                handle,
                self.aux.0,
                argv.len() as c_int,
                argv.as_ptr(),
                &mut vtab as *mut *mut RqlVtab,
                &mut err as *mut *mut c_char,
            )
        };
        if !err.is_null() {
            let msg = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
            unsafe { tramp_free(err as *mut c_void) };
            if rc != RQL_OK {
                return Err(Error::runtime(msg));
            }
        }
        if rc != RQL_OK {
            return Err(Error::runtime("xCreate failed".to_string()));
        }
        if vtab.is_null() {
            return Err(Error::runtime("xCreate returned NULL vtab".to_string()));
        }
        // Fill the engine-known header fields.
        unsafe {
            (*vtab).p_module = self.module.0;
            (*vtab).p_aux = self.aux.0;
        }
        let cols = DECLARED_VTAB
            .with(|d| d.borrow_mut().take())
            .ok_or_else(|| Error::runtime("xCreate did not call declare_vtab".to_string()))?;
        Ok((vtab, cols))
    }
}

impl crate::plugin::VirtualTableModule for CVtabModule {
    fn name(&self) -> &str {
        &self.name
    }

    fn caps(&self) -> u32 {
        let m = unsafe { &*self.module.0 };
        if m.x_update.is_some() {
            crate::plugin::vtab::ModuleCaps::WRITABLE
        } else {
            0
        }
    }

    fn create(&self, table: &str, args: &[String]) -> Result<Box<dyn crate::plugin::VirtualTable>> {
        let db = current_db_thread();
        if db.is_null() {
            return Err(Error::runtime(
                "CREATE VIRTUAL TABLE requires the engine's thread-db bridge".to_string(),
            ));
        }
        let (vtab, cols) = self.call_create_connect(db, table, args)?;
        Ok(Box::new(CVtab {
            module: ModulePtr(self.module.0),
            vtab_ptr: vtab,
            columns: cols,
        }))
    }

    fn connect(&self, table: &str, args: &[String]) -> Result<Box<dyn crate::plugin::VirtualTable>> {
        self.is_connect.store(true, std::sync::atomic::Ordering::Release);
        let db = current_db_thread();
        if db.is_null() {
            return Err(Error::runtime(
                "virtual table connect requires the engine's thread-db bridge".to_string(),
            ));
        }
        let (vtab, cols) = self.call_create_connect(db, table, args)?;
        Ok(Box::new(CVtab {
            module: ModulePtr(self.module.0),
            vtab_ptr: vtab,
            columns: cols,
        }))
    }

    fn destroy(&self, table: &str, args: &[String]) -> Result<()> {
        let m = unsafe { &*self.module.0 };
        if let Some(xd) = m.x_destroy {
            // Build the argv the same way as create (SQLite passes the
            // CREATE VIRTUAL TABLE argv to xDestroy).
            let mut argv_owned: Vec<CString> = Vec::with_capacity(3 + args.len());
            argv_owned.push(CString::new(self.name.clone()).unwrap());
            argv_owned.push(CString::new("db").unwrap());
            argv_owned.push(CString::new(table).unwrap());
            for a in args {
                argv_owned.push(CString::new(a.as_bytes().to_vec()).unwrap());
            }
            let argv: Vec<*const c_char> = argv_owned.iter().map(|c| c.as_ptr()).collect();
            let err: *mut c_char = std::ptr::null_mut();
            // xDestroy takes the vtab instance pointer (SQLite semantics:
            // the module frees its own vtab). We don't have it here (the
            // engine dropped the instance) — call with a minimal header.
            // NOTE: proper xDestroy needs the instance; the CVtab Drop
            // calls x_disconnect, and table-level destroy is invoked with
            // a synthesized header carrying the aux pointer.
            let header = RqlVtab {
                p_module: self.module.0,
                p_aux: self.aux.0,
                z_err_msg: std::ptr::null_mut(),
            };
            let rc = unsafe { xd(&header as *const RqlVtab as *mut RqlVtab) };
            let _ = argv;
            let _ = err;
            if rc != RQL_OK {
                return Err(Error::runtime("xDestroy failed".to_string()));
            }
        }
        Ok(())
    }
}

/// A `VirtualTable` backed by a C `rql_vtab` pointer.
struct CVtab {
    module: ModulePtr,
    vtab_ptr: *mut RqlVtab,
    columns: Vec<(String, String)>,
}

impl CVtab {
    #[allow(unused_unsafe)]
    fn with_err(&self, rc: c_int) -> Result<()> {
        if rc != RQL_OK {
            let msg = unsafe {
                if !(*self.vtab_ptr).z_err_msg.is_null() {
                    let m = CStr::from_ptr((*self.vtab_ptr).z_err_msg).to_string_lossy().into_owned();
                    tramp_free((*self.vtab_ptr).z_err_msg as *mut c_void);
                    (*self.vtab_ptr).z_err_msg = std::ptr::null_mut();
                    m
                } else {
                    format!("virtual table call failed (rc={})", rc)
                }
            };
            return Err(Error::runtime(msg));
        }
        Ok(())
    }
}

impl crate::plugin::VirtualTable for CVtab {
    fn columns(&self) -> Vec<(String, String)> {
        self.columns.clone()
    }

    fn best_index(&self, constraints: &[crate::plugin::vtab::VtabConstraint]) -> Result<crate::plugin::vtab::IndexInfo> {
        let m = unsafe { &*self.module.0 };
        let Some(xb) = m.x_best_index else {
            return Ok(crate::plugin::vtab::IndexInfo::full_scan(constraints.len()));
        };
        // Build the C constraint array.
        let c_constraints: Vec<RqlIndexConstraint> = constraints
            .iter()
            .map(|c| {
                let op = match c.op {
                    crate::plugin::vtab::VtabConstraintOp::Eq => 2,
                    crate::plugin::vtab::VtabConstraintOp::Lt => 16,
                    crate::plugin::vtab::VtabConstraintOp::Le => 8,
                    crate::plugin::vtab::VtabConstraintOp::Gt => 4,
                    crate::plugin::vtab::VtabConstraintOp::Ge => 32,
                    crate::plugin::vtab::VtabConstraintOp::Like => 66,
                    crate::plugin::vtab::VtabConstraintOp::Glob => 74,
                };
                RqlIndexConstraint {
                    column: c.column.map(|i| i as c_int).unwrap_or(-1),
                    op,
                    usable: 1,
                    _pad: [0; 3],
                }
            })
            .collect();
        let mut usage: Vec<u8> = vec![0; c_constraints.len()];
        let _ = &mut usage;
        let idx_str: *mut c_char;
        let mut info = RqlIndexInfo {
            n_constraint: c_constraints.len() as c_int,
            a_constraint: c_constraints.as_ptr(),
            idx_num: 0,
            idx_str: std::ptr::null_mut(),
            a_constraint_usage: usage.as_mut_ptr(),
            estimated_cost: 1e9,
            estimated_rows: 0,
        };
        let rc = unsafe { xb(self.vtab_ptr, &mut info as *mut RqlIndexInfo) };
        idx_str = info.idx_str;
        let mut out = crate::plugin::vtab::IndexInfo::full_scan(constraints.len());
        out.idx_num = info.idx_num as usize;
        out.estimated_cost = info.estimated_cost;
        out.estimated_rows = info.estimated_rows;
        out.handled = usage.into_iter().map(|u| u != 0).collect();
        if !idx_str.is_null() {
            out.idx_str = Some(unsafe { CStr::from_ptr(idx_str) }.to_string_lossy().into_owned());
            unsafe { tramp_free(idx_str as *mut c_void) };
        }
        self.with_err(rc)?;
        Ok(out)
    }

    fn open(&self) -> Result<Box<dyn crate::plugin::VirtualTableCursor>> {
        let m = unsafe { &*self.module.0 };
        let Some(xo) = m.x_open else {
            return Err(Error::Unsupported("vtab module has no xOpen"));
        };
        let mut cursor: *mut RqlVtabCursor = std::ptr::null_mut();
        let rc = unsafe { xo(self.vtab_ptr, &mut cursor as *mut *mut RqlVtabCursor) };
        self.with_err(rc)?;
        if cursor.is_null() {
            return Err(Error::runtime("xOpen returned NULL".to_string()));
        }
        Ok(Box::new(CCursor {
            module: ModulePtr(self.module.0),
            vtab_ptr: self.vtab_ptr,
            cursor_ptr: cursor,
        }))
    }

    fn update(&mut self, ops: Vec<crate::plugin::vtab::UpdateOp>) -> Result<Vec<Option<i64>>> {
        let m = unsafe { &*self.module.0 };
        let Some(xu) = m.x_update else {
            return Err(Error::Unsupported("virtual table is read-only"));
        };
        let mut out_rowids = Vec::with_capacity(ops.len());
        // One xUpdate call per op (SQLite batches within a statement; the
        // all-or-nothing semantics come from the engine's statement scope).
        for op in ops {
            // argv[0] = old rowid (NULL for insert), argv[1..] = columns.
            let mut values: Vec<Option<Value>> = Vec::with_capacity(1 + op.columns.len());
            values.push(op.old_rowid.map(Value::Integer));
            for c in &op.columns {
                values.push(c.clone());
            }
            let handles: Vec<Box<Value>> = values
                .into_iter()
                .map(|v| Box::new(v.unwrap_or(Value::Null)))
                .collect();
            let mut argv: Vec<*mut RqlValue> =
                handles.iter().map(|h| h.as_ref() as *const Value as *mut RqlValue).collect();
            // For DELETE SQLite passes argc == 1 with a non-NULL rowid.
            let argc = if op.columns.is_empty() && op.old_rowid.is_some() {
                1
            } else {
                argv.len()
            };
            let mut rowid_out: i64 = 0;
            let rc = unsafe { xu(self.vtab_ptr, argc as c_int, argv.as_mut_ptr(), &mut rowid_out) };
            self.with_err(rc)?;
            out_rowids.push(if op.old_rowid.is_none() { Some(rowid_out) } else { None });
        }
        Ok(out_rowids)
    }
}

impl Drop for CVtab {
    fn drop(&mut self) {
        let m = unsafe { &*self.module.0 };
        if let Some(xd) = m.x_disconnect {
            unsafe { xd(self.vtab_ptr) };
        }
    }
}

/// A `VirtualTableCursor` over a C `rql_vtab_cursor`.
struct CCursor {
    module: ModulePtr,
    vtab_ptr: *mut RqlVtab,
    cursor_ptr: *mut RqlVtabCursor,
}

impl crate::plugin::VirtualTableCursor for CCursor {
    fn filter(
        &mut self,
        idx_num: usize,
        idx_str: Option<&str>,
        args: &[Value],
    ) -> Result<()> {
        let m = unsafe { &*self.module.0 };
        let Some(xf) = m.x_filter else {
            return Err(Error::Unsupported("vtab module has no xFilter"));
        };
        let idx_c = idx_str.map(|s| CString::new(s.as_bytes().to_vec()).unwrap());
        let handles: Vec<Box<Value>> = args.iter().cloned().map(Box::new).collect();
        let mut argv: Vec<*mut RqlValue> =
            handles.iter().map(|h| h.as_ref() as *const Value as *mut RqlValue).collect();
        let rc = unsafe {
            xf(
                self.cursor_ptr,
                idx_num as c_int,
                idx_c.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null()),
                argv.len() as c_int,
                argv.as_mut_ptr(),
            )
        };
        drop(idx_c);
        drop(handles);
        if rc != RQL_OK {
            let msg = unsafe {
                if !(*self.vtab_ptr).z_err_msg.is_null() {
                    let m = CStr::from_ptr((*self.vtab_ptr).z_err_msg).to_string_lossy().into_owned();
                    tramp_free((*self.vtab_ptr).z_err_msg as *mut c_void);
                    (*self.vtab_ptr).z_err_msg = std::ptr::null_mut();
                    m
                } else {
                    format!("xFilter failed (rc={})", rc)
                }
            };
            return Err(Error::runtime(msg));
        }
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        let m = unsafe { &*self.module.0 };
        let rc = unsafe { m.x_next.unwrap()(self.cursor_ptr) };
        if rc != RQL_OK {
            let msg = unsafe {
                if !(*self.vtab_ptr).z_err_msg.is_null() {
                    let m = CStr::from_ptr((*self.vtab_ptr).z_err_msg).to_string_lossy().into_owned();
                    tramp_free((*self.vtab_ptr).z_err_msg as *mut c_void);
                    (*self.vtab_ptr).z_err_msg = std::ptr::null_mut();
                    m
                } else {
                    format!("xNext failed (rc={})", rc)
                }
            };
            return Err(Error::runtime(msg));
        }
        Ok(())
    }

    fn eof(&self) -> bool {
        let m = unsafe { &*self.module.0 };
        unsafe { m.x_eof.unwrap()(self.cursor_ptr) != 0 }
    }

    fn column(&self, i: usize) -> Result<Value> {
        let m = unsafe { &*self.module.0 };
        let Some(xc) = m.x_column else {
            return Ok(Value::Null);
        };
        let mut call = CallCtx {
            out: None,
            err: None,
            agg_mem: None,
            agg_len: 0,
            app: std::ptr::null_mut(),
            leaked: Vec::new(),
        };
        let call_ptr = &mut call as *mut CallCtx;
        set_call_ctx(call_ptr);
        let rc = unsafe { xc(self.cursor_ptr, call_ptr as *mut RqlContext, i as c_int) };
        clear_call_ctx();
        for p in call.leaked.drain(..) {
            unsafe { drop(CString::from_raw(p as *mut c_char)) };
        }
        if rc != RQL_OK {
            return Err(Error::runtime("xColumn failed".to_string()));
        }
        if let Some(e) = call.err {
            return Err(Error::runtime(e));
        }
        Ok(call.out.unwrap_or(Value::Null))
    }

    fn rowid(&self) -> Result<i64> {
        let m = unsafe { &*self.module.0 };
        let mut rid: i64 = 0;
        let rc = unsafe { m.x_rowid.unwrap()(self.cursor_ptr, &mut rid) };
        if rc != RQL_OK {
            return Err(Error::runtime("xRowid failed".to_string()));
        }
        Ok(rid)
    }
}

impl Drop for CCursor {
    fn drop(&mut self) {
        let m = unsafe { &*self.module.0 };
        if let Some(xc) = m.x_close {
            unsafe { xc(self.cursor_ptr) };
        }
    }
}

thread_local! {
    /// Raw database pointer reachable during module callbacks (xCreate
    /// needs it for declare_vtab routing). Set by api.rs around the
    /// statement dispatch that can reach vtab creation, and by the C ABI's
    /// exec path — the borrow is valid for the duration of the guard.
    static THREAD_DB: std::cell::RefCell<*mut crate::api::Database> =
        const { std::cell::RefCell::new(std::ptr::null_mut()) };
}

/// RAII guard installing the thread-local database pointer.
pub(crate) struct ThreadDbGuard {
    prev: *mut crate::api::Database,
}

impl ThreadDbGuard {
    pub(crate) fn install(db: *mut crate::api::Database) -> Self {
        let prev = THREAD_DB.with(|d| std::mem::replace(&mut *d.borrow_mut(), db));
        Self { prev }
    }
}

impl Drop for ThreadDbGuard {
    fn drop(&mut self) {
        THREAD_DB.with(|d| *d.borrow_mut() = self.prev);
    }
}

/// Borrow the thread-local database for module callbacks.
pub(crate) fn current_db_thread() -> *mut crate::api::Database {
    THREAD_DB.with(|d| *d.borrow())
}

// ---------------------------------------------------------------------------
// Public facade for the sqlite3 C ABI compatibility layer (compat crate)
// ---------------------------------------------------------------------------

/// `sqlite3_result_int` — writes into the call context.
pub fn api_result_int(ctx: *mut RqlContext, v: c_int) {
    let _ = with_ctx_ptr(ctx, |c| c.out = Some(Value::Integer(v as i64)));
}

/// `sqlite3_result_int64`.
pub fn api_result_int64(ctx: *mut RqlContext, v: i64) {
    let _ = with_ctx_ptr(ctx, |c| c.out = Some(Value::Integer(v)));
}

/// `sqlite3_result_double`.
pub fn api_result_double(ctx: *mut RqlContext, v: f64) {
    let _ = with_ctx_ptr(ctx, |c| c.out = Some(Value::Real(v)));
}

/// `sqlite3_result_null`.
pub fn api_result_null(ctx: *mut RqlContext) {
    let _ = with_ctx_ptr(ctx, |c| c.out = Some(Value::Null));
}

/// `sqlite3_result_text` (len < 0 = NUL-terminated).
pub fn api_result_text(ctx: *mut RqlContext, s: *const c_char, len: c_int) {
    #[allow(unused_unsafe)]
    unsafe {
    let bytes = cstr_or_len(s, len);
    let _ = with_ctx_ptr(ctx, |c| {
        c.out = Some(Value::Text(String::from_utf8_lossy(&bytes).into_owned().into()))
    });
    }
}

/// `sqlite3_result_blob`.
pub fn api_result_blob(ctx: *mut RqlContext, data: *const c_void, len: c_int) {
    #[allow(unused_unsafe)]
    unsafe {
    let bytes = if data.is_null() || len <= 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(data as *const u8, len as usize).to_vec()
    };
    let _ = with_ctx_ptr(ctx, |c| c.out = Some(Value::Blob(bytes)));
    }
}

/// `sqlite3_result_error`.
pub fn api_result_error(ctx: *mut RqlContext, msg: *const c_char, len: c_int) {
    #[allow(unused_unsafe)]
    unsafe {
    let bytes = cstr_or_len(msg, len);
    let _ = with_ctx_ptr(ctx, |c| {
        c.err = Some(String::from_utf8_lossy(&bytes).into_owned())
    });
    }
}

/// `sqlite3_user_data` — the p_app pointer registered with the function.
pub fn api_user_data(ctx: *mut RqlContext) -> *mut c_void {
    with_ctx_ptr(ctx, |c| c.app).unwrap_or(std::ptr::null_mut())
}

/// `sqlite3_aggregate_context` — plugin-managed scratch memory.
pub fn api_aggregate_context(ctx: *mut RqlContext, n_bytes: c_int) -> *mut c_void {
    unsafe { tramp_aggregate_context(ctx, n_bytes) }
}
