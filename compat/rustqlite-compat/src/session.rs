//! The session extension's C ABI — `sqlite3session_*`,
//! `sqlite3changeset_*`, `sqlite3changegroup_*`, `sqlite3rebaser_*`
//! (SQLite's session extension, backed by `rustqlite::session`).
//!
//! Object model (the crate's opaque-handle convention):
//! * `sqlite3_session*` → `Box<SessionObj>` (the session core + its
//!   engine reference — the engine keeps the registry alive);
//! * `sqlite3_changeset_iter*` → `Box<CsIterObj>` (an OWNED changeset
//!   buffer + the engine iterator; `changeset_conflict` serves the
//!   apply-time conflicting row);
//! * `sqlite3_changegroup*` / `sqlite3_rebaser*` → plain boxes.
//!
//! Per-connection capture: `sqlite3session_create` registers the
//! session under the CREATING handle's connection identity (the
//! `ConnIdentityGuard` the engine's statement scope consults), so a
//! session sees only its own connection's writes — exactly SQLite's
//! model with one shared engine per file.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::Arc;

use rustqlite::session as eng;
use rustqlite::types::Value;

use crate::{
    sqlite3, sqlite3_value, Conn, Engine, SQLITE_ABORT, SQLITE_CORRUPT, SQLITE_DONE, SQLITE_ERROR,
    SQLITE_MISUSE, SQLITE_NOMEM, SQLITE_OK, SQLITE_RANGE, SQLITE_ROW,
};

// ---- Opaque C handles ------------------------------------------------

#[repr(C)]
pub struct sqlite3_session {
    _private: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_changeset_iter {
    _private: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_changegroup {
    _private: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_rebaser {
    _private: [u8; 0],
}

// ---- Start / apply flag mirrors --------------------------------------

pub const SQLITE_CHANGESETSTART_INVERT: c_int = 0x0002;
pub const SQLITE_CHANGESETAPPLY_NOSAVEPOINT: c_int = 0x0001;
pub const SQLITE_CHANGESETAPPLY_INVERT: c_int = 0x0002;
pub const SQLITE_CHANGESETAPPLY_IGNORENOOP: c_int = 0x0004;
pub const SQLITE_CHANGESETAPPLY_FKNOACTION: c_int = 0x0008;

/// apply_v2's rebase output (our APPLY_REBASE mirror).
const APPLY_REBASE_ENGINE: u32 = 0x0010;

// Conflict codes / verdicts (sqlite3.h values).
pub const SQLITE_CHANGESET_DATA: c_int = 1;
pub const SQLITE_CHANGESET_NOTFOUND: c_int = 2;
pub const SQLITE_CHANGESET_CONFLICT: c_int = 3;
pub const SQLITE_CHANGESET_CONSTRAINT: c_int = 4;
pub const SQLITE_CHANGESET_FOREIGN_KEY: c_int = 5;
pub const SQLITE_CHANGESET_OMIT: c_int = 1;
pub const SQLITE_CHANGESET_REPLACE: c_int = 5;
pub const SQLITE_CHANGESET_ABORT: c_int = 18;

// ---- Backing objects --------------------------------------------------

struct SessionObj {
    session: eng::Session,
    #[allow(dead_code)]
    engine: Arc<Engine>,
}

struct CsIterObj {
    iter: eng::ChangesetIter<'static>,
    /// Value objects handed out by old/new/conflict — freed at
    /// next()/finalize (SQLite: valid until the next advance).
    values: Vec<*mut sqlite3_value>,
    /// The apply-time conflicting row (`sqlite3changeset_conflict`).
    conflict: Option<Vec<Value>>,
    /// The cached table-name C string (op hands out a pointer that
    /// stays valid until the next advance / finalize).
    tab_cstr: Option<std::ffi::CString>,
}

struct GroupObj(eng::ChangeGroup);
struct RebaserObj(eng::Rebaser);

// ---- sqlite3session_* --------------------------------------------------

/// Create a session bound to `db`'s `zDb` schema ("main" only here).
#[no_mangle]
pub unsafe extern "C" fn sqlite3session_create(
    db: *mut sqlite3,
    z_db: *const c_char,
    pp: *mut *mut sqlite3_session,
) -> c_int {
    if pp.is_null() {
        return SQLITE_MISUSE;
    }
    *pp = std::ptr::null_mut();
    let Some(conn) = (db as *const Conn).as_ref() else {
        return SQLITE_MISUSE;
    };
    let Some(engine) = conn.engine.clone() else {
        return SQLITE_MISUSE;
    };
    let zdb = if z_db.is_null() {
        "main".to_string()
    } else {
        CStr::from_ptr(z_db).to_string_lossy().into_owned()
    };
    // Register under THIS connection's identity so the engine's
    // statement scope captures only this handle's writes.
    let session = {
        let _id = rustqlite::ConnIdentityGuard::arm(conn.id as u64);
        let mut w = engine.db.write();
        w.create_session()
    };
    {
        let mut core = session.core().lock().unwrap();
        core.z_db = zdb;
    }
    let obj = Box::new(SessionObj {
        session,
        engine: engine.clone(),
    });
    *pp = Box::into_raw(obj) as *mut sqlite3_session;
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_delete(p: *mut sqlite3_session) {
    if !p.is_null() {
        drop(Box::from_raw(p as *mut SessionObj));
    }
}

/// Enable/disable recording; returns the PREVIOUS state (-1 on misuse).
#[no_mangle]
pub unsafe extern "C" fn sqlite3session_enable(p: *mut sqlite3_session, on: c_int) -> c_int {
    let Some(obj) = (p as *const SessionObj).as_ref() else {
        return -1;
    };
    let mut core = obj.session.core().lock().unwrap();
    let prev = core.enable;
    core.enable = on != 0;
    prev as c_int
}

/// Mark subsequent changes indirect; returns the previous flag.
#[no_mangle]
pub unsafe extern "C" fn sqlite3session_indirect(p: *mut sqlite3_session, on: c_int) -> c_int {
    let Some(obj) = (p as *const SessionObj).as_ref() else {
        return -1;
    };
    let mut core = obj.session.core().lock().unwrap();
    let prev = core.indirect;
    core.indirect = on != 0;
    prev as c_int
}

/// Attach one table (NULL = every table, including future ones).
#[no_mangle]
pub unsafe extern "C" fn sqlite3session_attach(
    p: *mut sqlite3_session,
    z_tab: *const c_char,
) -> c_int {
    let Some(obj) = (p as *const SessionObj).as_ref() else {
        return SQLITE_MISUSE;
    };
    let name = if z_tab.is_null() {
        None
    } else {
        Some(CStr::from_ptr(z_tab).to_string_lossy().into_owned())
    };
    obj.session.core().lock().unwrap().attach(name.as_deref());
    SQLITE_OK
}

type TableFilterCb = Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int>;

/// A C table-filter callback + its user data — Send by SQLite's
/// contract (valid until the session is deleted).
struct CFilter {
    f: unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int,
    ctx: *mut c_void,
}
// SAFETY: the callback and ctx stay alive until sqlite3session_delete
// (SQLite's documented lifetime); the engine only invokes the filter on
// the writing thread inside the session's mutex.
unsafe impl Send for CFilter {}
impl CFilter {
    fn call(&self, name: &str) -> bool {
        let cname = std::ffi::CString::new(name).unwrap_or_default();
        unsafe { (self.f)(self.ctx, cname.as_ptr()) != 0 }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_table_filter(
    p: *mut sqlite3_session,
    x_filter: TableFilterCb,
    ctx: *mut c_void,
) {
    let Some(obj) = (p as *const SessionObj).as_ref() else {
        return;
    };
    match x_filter {
        None => obj.session.set_table_filter::<fn(&str) -> bool>(None),
        Some(f) => {
            let cf = CFilter { f, ctx };
            obj.session
                .set_table_filter(Some(move |name: &str| cf.call(name)));
        }
    }
}

/// `sqlite3session_diff` — schema-diff two databases. This engine is
/// single-database (no ATTACH); a non-main zFromDb has no schema to
/// diff against, which is exactly SQLite's error for an unknown
/// database.
#[no_mangle]
pub unsafe extern "C" fn sqlite3session_diff(
    p: *mut sqlite3_session,
    z_from: *const c_char,
    _z_tbl: *const c_char,
    pz_errmsg: *mut *mut c_char,
) -> c_int {
    let _ = p;
    if !pz_errmsg.is_null() {
        *pz_errmsg = std::ptr::null_mut();
    }
    let from = if z_from.is_null() {
        String::new()
    } else {
        CStr::from_ptr(z_from).to_string_lossy().into_owned()
    };
    let msg = format!("unable to open database: {from}");
    if !pz_errmsg.is_null() {
        if let Ok(c) = std::ffi::CString::new(msg) {
            let buf = crate::sqlite_alloc(c.as_bytes().len() + 1);
            if !buf.is_null() {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        c.as_ptr(),
                        buf as *mut c_char,
                        c.as_bytes().len() + 1,
                    );
                }
                *pz_errmsg = buf as *mut c_char;
            }
        }
    }
    SQLITE_ERROR
}

/// Collect the recorded changes into a changeset buffer (freed with
/// `sqlite3_free`).
#[no_mangle]
pub unsafe extern "C" fn sqlite3session_changeset(
    p: *mut sqlite3_session,
    pn: *mut c_int,
    pp: *mut *mut c_void,
) -> c_int {
    session_collect(p, pn, pp, false)
}

/// The patchset form.
#[no_mangle]
pub unsafe extern "C" fn sqlite3session_patchset(
    p: *mut sqlite3_session,
    pn: *mut c_int,
    pp: *mut *mut c_void,
) -> c_int {
    session_collect(p, pn, pp, true)
}

unsafe fn session_collect(
    p: *mut sqlite3_session,
    pn: *mut c_int,
    pp: *mut *mut c_void,
    patchset: bool,
) -> c_int {
    if pn.is_null() || pp.is_null() {
        return SQLITE_MISUSE;
    }
    *pn = 0;
    *pp = std::ptr::null_mut();
    let Some(obj) = (p as *const SessionObj).as_ref() else {
        return SQLITE_MISUSE;
    };
    let r = {
        let guard = obj.engine.db.read();
        if patchset {
            obj.session.patchset(&guard)
        } else {
            obj.session.changeset(&guard)
        }
    };
    match r {
        Ok(cs) => unsafe { copy_out(&cs, pn, pp) },
        Err(e) => {
            // The error text routes through the session's engine
            // connection state where possible; the result code is
            // SQLITE_ERROR for schema-class failures.
            let _ = e;
            SQLITE_ERROR
        }
    }
}

unsafe fn copy_out(bytes: &[u8], pn: *mut c_int, pp: *mut *mut c_void) -> c_int {
    let buf = crate::sqlite_alloc(bytes.len().max(1));
    if buf.is_null() {
        return SQLITE_NOMEM;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len());
    }
    *pn = bytes.len() as c_int;
    *pp = buf;
    SQLITE_OK
}

/// `sqlite3session_object_config` — SQLITE_SESSION_OBJCONFIG_SIZE (1)
/// / OBJCONFIG_ROWID (2). pArg: >= 0 sets, < 0 queries; the variable
/// receives the current value. MISUSE after the first attach or for an
/// unknown op.
#[no_mangle]
pub unsafe extern "C" fn sqlite3session_object_config(
    p: *mut sqlite3_session,
    op: c_int,
    p_arg: *mut c_int,
) -> c_int {
    let Some(obj) = (p as *const SessionObj).as_ref() else {
        return SQLITE_MISUSE;
    };
    if p_arg.is_null() {
        return SQLITE_MISUSE;
    }
    let mut arg = unsafe { *p_arg };
    let mut core = obj.session.core().lock().unwrap();
    if core.object_config(op, &mut arg) {
        unsafe { *p_arg = arg };
        SQLITE_OK
    } else {
        SQLITE_MISUSE
    }
}

/// `sqlite3session_changeset_size` — a byte estimate of the pending
/// changeset (requires OBJCONFIG_SIZE; 0 otherwise).
#[no_mangle]
pub unsafe extern "C" fn sqlite3session_changeset_size(p: *mut sqlite3_session) -> i64 {
    let Some(obj) = (p as *const SessionObj).as_ref() else {
        return 0;
    };
    let core = obj.session.core().lock().unwrap();
    core.changeset_size() as i64
}

/// `sqlite3session_config` — the process-wide config. Only
/// SQLITE_SESSION_CONFIG_STRMSIZE (the streaming chunk size) exists;
/// accepted and stored (the engine's API is buffer-based, so it has no
/// behavioral effect).
#[no_mangle]
pub unsafe extern "C" fn sqlite3session_config(op: c_int, p_arg: *mut c_void) -> c_int {
    match op {
        1 => {
            // STRMSIZE: *(int*)pArg — store nothing; buffer API.
            let _ = p_arg;
            SQLITE_OK
        }
        _ => SQLITE_MISUSE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_isempty(p: *mut sqlite3_session) -> c_int {
    let Some(obj) = (p as *const SessionObj).as_ref() else {
        return 1;
    };
    obj.session.core().lock().unwrap().is_empty() as c_int
}

// ---- sqlite3changeset_* (iterator) -------------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_start(
    pp: *mut *mut sqlite3_changeset_iter,
    n: c_int,
    p: *const c_void,
) -> c_int {
    changeset_start(pp, n, p, 0)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_start_v2(
    pp: *mut *mut sqlite3_changeset_iter,
    n: c_int,
    p: *const c_void,
    flags: c_int,
) -> c_int {
    changeset_start(pp, n, p, flags)
}

unsafe fn changeset_start(
    pp: *mut *mut sqlite3_changeset_iter,
    n: c_int,
    p: *const c_void,
    flags: c_int,
) -> c_int {
    if pp.is_null() {
        return SQLITE_MISUSE;
    }
    *pp = std::ptr::null_mut();
    if n < 0 || (n > 0 && p.is_null()) {
        return SQLITE_CORRUPT;
    }
    let data = unsafe { std::slice::from_raw_parts(p as *const u8, n as usize) }.to_vec();
    let eng_flags = if flags & SQLITE_CHANGESETSTART_INVERT != 0 {
        eng::iter::CHANGESETSTART_INVERT
    } else {
        0
    };
    match eng::ChangesetIter::start_owned(data, eng_flags) {
        Ok(iter) => {
            let obj = Box::new(CsIterObj {
                iter,
                values: Vec::new(),
                conflict: None,
                tab_cstr: None,
            });
            *pp = Box::into_raw(obj) as *mut sqlite3_changeset_iter;
            SQLITE_OK
        }
        Err(_) => SQLITE_CORRUPT,
    }
}

/// Advance: SQLITE_ROW (a change is available) or SQLITE_DONE.
#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_next(p: *mut sqlite3_changeset_iter) -> c_int {
    let Some(obj) = (p as *mut CsIterObj).as_mut() else {
        return SQLITE_MISUSE;
    };
    free_value_batch(&mut obj.values);
    obj.conflict = None;
    obj.tab_cstr = None;
    match obj.iter.next() {
        Ok(true) => SQLITE_ROW,
        Ok(false) => SQLITE_DONE,
        Err(_) => SQLITE_CORRUPT,
    }
}

unsafe fn free_value_batch(values: &mut Vec<*mut sqlite3_value>) {
    for v in values.drain(..) {
        drop(unsafe { Box::from_raw(v as *mut Value) });
    }
}

/// The current change's table / column count / op / indirect flag.
#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_op(
    p: *mut sqlite3_changeset_iter,
    pz_tab: *mut *const c_char,
    pn_col: *mut c_int,
    p_op: *mut c_int,
    pb_indirect: *mut c_int,
) -> c_int {
    let Some(obj) = (p as *const CsIterObj).as_ref() else {
        return SQLITE_MISUSE;
    };
    let _ = obj;
    if !pz_tab.is_null() {
        let obj = &mut *(p as *mut CsIterObj);
        let name = obj.iter.table().to_string();
        obj.tab_cstr = std::ffi::CString::new(name).ok();
        *pz_tab = obj
            .tab_cstr
            .as_ref()
            .map(|c| c.as_ptr())
            .unwrap_or(std::ptr::null());
    }
    if !pn_col.is_null() {
        *pn_col = obj.iter.n_col() as c_int;
    }
    if !p_op.is_null() {
        *p_op = obj.iter.op() as c_int;
    }
    if !pb_indirect.is_null() {
        *pb_indirect = obj.iter.indirect() as c_int;
    }
    SQLITE_OK
}

/// The PK bitmap of the current table.
#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_pk(
    p: *mut sqlite3_changeset_iter,
    ppab_pk: *mut *mut u8,
    pn_col: *mut c_int,
) -> c_int {
    let Some(obj) = (p as *const CsIterObj).as_ref() else {
        return SQLITE_MISUSE;
    };
    if !ppab_pk.is_null() {
        let buf = crate::sqlite_alloc(obj.iter.pk().len().max(1));
        if buf.is_null() {
            return SQLITE_NOMEM;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                obj.iter.pk().as_ptr(),
                buf as *mut u8,
                obj.iter.pk().len(),
            );
        }
        *ppab_pk = buf as *mut u8;
    }
    if !pn_col.is_null() {
        *pn_col = obj.iter.n_col() as c_int;
    }
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_old(
    p: *mut sqlite3_changeset_iter,
    i: c_int,
    pp: *mut *mut sqlite3_value,
) -> c_int {
    changeset_side(p, i, pp, false, false)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_new(
    p: *mut sqlite3_changeset_iter,
    i: c_int,
    pp: *mut *mut sqlite3_value,
) -> c_int {
    changeset_side(p, i, pp, true, false)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_conflict(
    p: *mut sqlite3_changeset_iter,
    i: c_int,
    pp: *mut *mut sqlite3_value,
) -> c_int {
    changeset_side(p, i, pp, false, true)
}

unsafe fn changeset_side(
    p: *mut sqlite3_changeset_iter,
    i: c_int,
    pp: *mut *mut sqlite3_value,
    new_side: bool,
    conflict: bool,
) -> c_int {
    if pp.is_null() {
        return SQLITE_MISUSE;
    }
    *pp = std::ptr::null_mut();
    let Some(obj) = (p as *mut CsIterObj).as_mut() else {
        return SQLITE_MISUSE;
    };
    if i < 0 || i as usize >= obj.iter.n_col() {
        return SQLITE_RANGE;
    }
    let val: Option<Value> = if conflict {
        obj.conflict
            .as_ref()
            .and_then(|row| row.get(i as usize).cloned())
    } else {
        let side = if new_side {
            obj.iter.new_val(i as usize)
        } else {
            obj.iter.old(i as usize)
        };
        side.and_then(|v| v.to_value())
    };
    // Undefined field (or no conflict row): SQLite hands back a NULL
    // value object.
    let val = val.unwrap_or(Value::Null);
    let pv = Box::into_raw(Box::new(val)) as *mut sqlite3_value;
    obj.values.push(pv);
    *pp = pv;
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_finalize(p: *mut sqlite3_changeset_iter) -> c_int {
    if p.is_null() {
        return SQLITE_MISUSE;
    }
    let mut obj = unsafe { Box::from_raw(p as *mut CsIterObj) };
    unsafe { free_value_batch(&mut obj.values) };
    SQLITE_OK
}

// ---- sqlite3changeset_invert / concat ----------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_invert(
    n: c_int,
    p: *const c_void,
    pn_out: *mut c_int,
    pp_out: *mut *mut c_void,
) -> c_int {
    if pn_out.is_null() || pp_out.is_null() {
        return SQLITE_MISUSE;
    }
    *pn_out = 0;
    *pp_out = std::ptr::null_mut();
    if n < 0 || (n > 0 && p.is_null()) {
        return SQLITE_CORRUPT;
    }
    let data = unsafe { std::slice::from_raw_parts(p as *const u8, n as usize) };
    match eng::changeset_invert(data) {
        Ok(out) => unsafe { copy_out(&out, pn_out, pp_out) },
        Err(_) => SQLITE_CORRUPT,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_concat(
    n_a: c_int,
    p_a: *const c_void,
    n_b: c_int,
    p_b: *const c_void,
    pn_out: *mut c_int,
    pp_out: *mut *mut c_void,
) -> c_int {
    if pn_out.is_null() || pp_out.is_null() {
        return SQLITE_MISUSE;
    }
    *pn_out = 0;
    *pp_out = std::ptr::null_mut();
    if n_a < 0 || n_b < 0 {
        return SQLITE_CORRUPT;
    }
    let a = unsafe { std::slice::from_raw_parts(p_a as *const u8, n_a as usize) };
    let b = unsafe { std::slice::from_raw_parts(p_b as *const u8, n_b as usize) };
    match eng::changeset_concat(a, b) {
        Ok(out) => unsafe { copy_out(&out, pn_out, pp_out) },
        Err(_) => SQLITE_CORRUPT,
    }
}

// ---- sqlite3changeset_apply / _v2 ---------------------------------------

type ConflictCb = unsafe extern "C" fn(*mut c_void, c_int, *mut sqlite3_changeset_iter) -> c_int;
type FilterCb = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_apply(
    db: *mut sqlite3,
    n: c_int,
    p: *const c_void,
    x_filter: Option<FilterCb>,
    x_conflict: ConflictCb,
    ctx: *mut c_void,
) -> c_int {
    unsafe {
        sqlite3changeset_apply_v2(
            db,
            n,
            p,
            x_filter,
            x_conflict,
            ctx,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_apply_v2(
    db: *mut sqlite3,
    n: c_int,
    p: *const c_void,
    x_filter: Option<FilterCb>,
    x_conflict: ConflictCb,
    ctx: *mut c_void,
    pp_rebase: *mut *mut c_void,
    pn_rebase: *mut c_int,
    flags: c_int,
) -> c_int {
    let Some(conn) = (db as *const Conn).as_ref() else {
        return SQLITE_MISUSE;
    };
    let Some(engine) = conn.engine.clone() else {
        return SQLITE_MISUSE;
    };
    if n < 0 || (n > 0 && p.is_null()) {
        return SQLITE_CORRUPT;
    }
    let data = unsafe { std::slice::from_raw_parts(p as *const u8, n as usize) };
    if (!pp_rebase.is_null()) != (!pn_rebase.is_null()) {
        return SQLITE_MISUSE;
    }
    let want_rebase = !pp_rebase.is_null();
    if want_rebase {
        *pp_rebase = std::ptr::null_mut();
        *pn_rebase = 0;
    }

    // The conflict callback receives a live iterator handle whose
    // op/old/new/conflict mirror the event.
    let mut mirror: Box<CsIterObj> = Box::new(CsIterObj {
        iter: eng::ChangesetIter::start_owned(Vec::new(), 0).unwrap(),
        values: Vec::new(),
        conflict: None,
        tab_cstr: None,
    });

    let mut eng_flags: u32 = 0;
    if flags & SQLITE_CHANGESETAPPLY_NOSAVEPOINT != 0 {
        eng_flags |= eng::APPLY_NOSAVEPOINT;
    }
    if flags & SQLITE_CHANGESETAPPLY_INVERT != 0 {
        eng_flags |= eng::APPLY_INVERT;
    }
    if flags & SQLITE_CHANGESETAPPLY_IGNORENOOP != 0 {
        eng_flags |= eng::APPLY_IGNORENOOP;
    }
    if flags & SQLITE_CHANGESETAPPLY_FKNOACTION != 0 {
        eng_flags |= eng::APPLY_FKNOACTION;
    }
    if want_rebase {
        eng_flags |= APPLY_REBASE_ENGINE;
    }

    // Bridge the engine conflict event into the C callback. The mirror
    // iterator is rebuilt per event (a synthetic one-change changeset)
    // so op/old/new/conflict serve the CURRENT change honestly.
    let ctx_cell = ctx;
    let cb = x_conflict;
    let mirror_ptr: *mut CsIterObj = &mut *mirror;
    let mut bridge = |evt: &eng::ConflictEvent<'_>| -> i32 {
        unsafe {
            let m = &mut *mirror_ptr;
            free_value_batch(&mut m.values);
            m.conflict = evt.conflict_row.clone();
            let cs = eng::synthetic_change(
                evt.table,
                evt.n_col,
                evt.pk,
                evt.op,
                evt.indirect,
                evt.old,
                evt.new,
            );
            if let Ok(mut it) = eng::ChangesetIter::start_owned(cs, 0) {
                if it.next().unwrap_or(false) {
                    m.iter = it;
                }
            }
            cb(
                ctx_cell,
                evt.code,
                mirror_ptr as *mut sqlite3_changeset_iter,
            )
        }
    };

    // Bridge the filter.
    type FilterBox = Box<dyn FnMut(&str) -> bool>;
    let mut filter_box: Option<FilterBox> = None;
    if let Some(f) = x_filter {
        filter_box = Some(Box::new(move |name: &str| -> bool {
            let cname = std::ffi::CString::new(name).unwrap_or_default();
            unsafe { f(ctx, cname.as_ptr()) != 0 }
        }));
    }

    // Apply through THIS connection's identity (the applying
    // connection's own sessions record the applied changes, like
    // SQLite).
    let result = {
        let _id = rustqlite::ConnIdentityGuard::arm(conn.id as u64);
        let mut w = engine.db.write();
        eng::changeset_apply(
            &mut w,
            data,
            eng_flags,
            filter_box
                .as_mut()
                .map(|b| &mut **b as &mut dyn FnMut(&str) -> bool),
            &mut bridge,
        )
    };
    match result {
        Ok(rebase) => {
            if want_rebase {
                if let Some(rb) = rebase {
                    unsafe { copy_out(&rb, pn_rebase, pp_rebase) }
                } else {
                    SQLITE_OK
                }
            } else {
                SQLITE_OK
            }
        }
        Err(e) => match e {
            eng::ApplyError::Engine(_) => SQLITE_ERROR,
            eng::ApplyError::Corrupt(_) => SQLITE_CORRUPT,
            eng::ApplyError::Misuse(_) => SQLITE_MISUSE,
            eng::ApplyError::Abort => SQLITE_ABORT,
        },
    }
}

// ---- sqlite3changegroup_* ----------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_new(pp: *mut *mut sqlite3_changegroup) -> c_int {
    if pp.is_null() {
        return SQLITE_MISUSE;
    }
    *pp = Box::into_raw(Box::new(GroupObj(eng::ChangeGroup::new()))) as *mut sqlite3_changegroup;
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_add(
    p: *mut sqlite3_changegroup,
    n: c_int,
    data: *const c_void,
) -> c_int {
    let Some(obj) = (p as *mut GroupObj).as_mut() else {
        return SQLITE_MISUSE;
    };
    if n < 0 || (n > 0 && data.is_null()) {
        return SQLITE_CORRUPT;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, n as usize) };
    match obj.0.add(bytes) {
        Ok(()) => SQLITE_OK,
        Err(_) => SQLITE_CORRUPT,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_output(
    p: *mut sqlite3_changegroup,
    pn: *mut c_int,
    pp: *mut *mut c_void,
) -> c_int {
    let Some(obj) = (p as *const GroupObj).as_ref() else {
        return SQLITE_MISUSE;
    };
    if pn.is_null() || pp.is_null() {
        return SQLITE_MISUSE;
    }
    let out = obj.0.output();
    unsafe { copy_out(&out, pn, pp) }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_delete(p: *mut sqlite3_changegroup) {
    if !p.is_null() {
        drop(unsafe { Box::from_raw(p as *mut GroupObj) });
    }
}

// ---- sqlite3rebaser_* ---------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3rebaser_create(pp: *mut *mut sqlite3_rebaser) -> c_int {
    if pp.is_null() {
        return SQLITE_MISUSE;
    }
    *pp = Box::into_raw(Box::new(RebaserObj(eng::Rebaser::new()))) as *mut sqlite3_rebaser;
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3rebaser_configure(
    p: *mut sqlite3_rebaser,
    n: c_int,
    data: *const c_void,
) -> c_int {
    let Some(obj) = (p as *mut RebaserObj).as_mut() else {
        return SQLITE_MISUSE;
    };
    if n < 0 || (n > 0 && data.is_null()) {
        return SQLITE_CORRUPT;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, n as usize) };
    match obj.0.configure(bytes) {
        Ok(()) => SQLITE_OK,
        Err(_) => SQLITE_CORRUPT,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3rebaser_rebase(
    p: *mut sqlite3_rebaser,
    n_in: c_int,
    p_in: *const c_void,
    pn_out: *mut c_int,
    pp_out: *mut *mut c_void,
) -> c_int {
    let Some(obj) = (p as *const RebaserObj).as_ref() else {
        return SQLITE_MISUSE;
    };
    if n_in < 0 || (n_in > 0 && p_in.is_null()) || pn_out.is_null() || pp_out.is_null() {
        return SQLITE_MISUSE;
    }
    *pn_out = 0;
    *pp_out = std::ptr::null_mut();
    let bytes = unsafe { std::slice::from_raw_parts(p_in as *const u8, n_in as usize) };
    match obj.0.rebase(bytes) {
        Ok(out) => unsafe { copy_out(&out, pn_out, pp_out) },
        Err(_) => SQLITE_CORRUPT,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3rebaser_delete(p: *mut sqlite3_rebaser) {
    if !p.is_null() {
        drop(unsafe { Box::from_raw(p as *mut RebaserObj) });
    }
}
