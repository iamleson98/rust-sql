//! The shell's file I/O + listing SQL function family — SQLite
//! fileio.c's functions, ported (the sqlite3 shell links them into
//! every session; rustqlite's CLI does the same).
//!
//! * `lsmode(mode)` — the `ls -l` style 10-char rendering (SQLite's
//!   exact format, including the `?` for unknown file types).
//! * `realpath(path)` — symlink-resolving absolute path (the C
//!   `realpath()` first; lexical cwd-join + `..` normalization when
//!   the file does not exist — SQLite's own fallback).
//! * `readfile(path)` — the file's bytes as a BLOB (NULL when
//!   missing; SQLite's TOOBIG error class for directories).
//! * `writefile(path, data[, mode[, mtime]])` — write bytes, return
//!   the byte count; `mode`/`mtime` apply chmod/utime (SQLite's
//!   exact 2/3/4-arg surface).
//! * `shell_putsnl(x)` — print `x` + newline to the CLI's output
//!   stream, return `x` (ar.c's verbose SELECT-list trick).
//! * `sqlar_compress(data)` / `sqlar_uncompress(data, sz)` — the
//!   sqlar codec. Compression: OUR writer stores raw members (the
//!   no-zlib sqlite3 build's discipline — see the zipfile module
//!   docs), so `sqlar_compress` is the identity, and
//!   `sqlar_uncompress` handles BOTH shapes (`len(data)==sz` → raw;
//!   otherwise a zlib stream inflated by the engine's own
//!   RFC-1951 inflater — byte-exact decompression is deterministic).
//! * `sqlar_uncompress` on a raw member returns it unchanged, so
//!   archives written by BOTH the zlib-linked shell and the no-zlib
//!   build (and ours) extract identically.

use crate::error::{Error, Result};
use crate::plugin::{Arity, FnCtx, ScalarFunction};
use crate::types::Value;
use std::sync::{Arc, Mutex};

/// The CLI's output sink for `shell_putsnl` (the SQL layer cannot see
/// the CLI's writer; the CLI installs the sink around .archive
/// execution — sqlite3's shell_putsnl writes through sqlite3_printf,
/// which likewise targets the shell's redirected output).
pub type PutsSink = Mutex<Box<dyn FnMut(&str) + Send>>;

static PUTS_SINK: Mutex<Option<Arc<PutsSink>>> = Mutex::new(None);

/// Install (or clear) the shell_putsnl sink.
pub fn set_puts_sink(sink: Option<Arc<PutsSink>>) {
    *PUTS_SINK.lock().unwrap_or_else(|e| e.into_inner()) = sink;
}

fn puts_line(s: &str) {
    let g = PUTS_SINK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(sink) = g.as_ref() {
        let mut f = sink.lock().unwrap_or_else(|e| e.into_inner());
        f(s);
    }
}

// ============================================================
// lsmode

/// SQLite's lsmode: the 10-char `ls -l` style rendering.
pub(crate) fn ls_mode(mode: i64) -> String {
    let mut out = String::with_capacity(10);
    // fileio.c's lsmode knows exactly three types (dir/link/regular);
    // everything else — devices, fifos, sockets, unknown — renders '?'
    // (oracle-pinned: lsmode(8630) = "?rw-rw-rw-", a char device).
    let typ = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o100000 => '-',
        _ => '?',
    };
    out.push(typ);
    let bits = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    for (bit, ch) in bits {
        out.push(if mode & bit != 0 { ch } else { '-' });
    }
    out
}

struct LsModeFn;
impl ScalarFunction for LsModeFn {
    fn name(&self) -> &str {
        "lsmode"
    }
    fn arity(&self) -> Arity {
        Arity::Exact(1)
    }
    fn deterministic(&self) -> bool {
        true
    }
    fn call(&self, _ctx: &FnCtx, args: &[Value]) -> Result<Value> {
        match &args[0] {
            Value::Null => Ok(Value::Text("?---------".into())),
            v => match Some(v.as_integer()) {
                Some(m) => Ok(Value::Text(ls_mode(m).into())),
                None => Ok(Value::Text("?---------".into())),
            },
        }
    }
}

// ============================================================
// realpath

pub(crate) fn real_path(p: &str) -> String {
    let path = std::path::Path::new(p);
    // Physical resolution first (symlinks resolved) — the C realpath.
    if let Ok(c) = std::fs::canonicalize(path) {
        return c.to_string_lossy().into_owned();
    }
    // SQLite's fallback: cwd-join + lexical normalization.
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    for c in joined.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str().to_os_string()),
        }
    }
    let mut s = std::path::PathBuf::new();
    for c in out {
        s.push(c);
    }
    s.to_string_lossy().into_owned()
}

struct RealPathFn;
impl ScalarFunction for RealPathFn {
    fn name(&self) -> &str {
        "realpath"
    }
    fn arity(&self) -> Arity {
        Arity::Exact(1)
    }
    fn call(&self, _ctx: &FnCtx, args: &[Value]) -> Result<Value> {
        match &args[0] {
            Value::Null => Ok(Value::Null),
            v => {
                let s = match v {
                    Value::Text(t) => t.as_str().to_string(),
                    other => other.to_string(),
                };
                Ok(Value::Text(real_path(&s).into()))
            }
        }
    }
}

// ============================================================
// readfile / writefile

struct ReadFileFn;
impl ScalarFunction for ReadFileFn {
    fn name(&self) -> &str {
        "readfile"
    }
    fn arity(&self) -> Arity {
        Arity::Exact(1)
    }
    fn call(&self, _ctx: &FnCtx, args: &[Value]) -> Result<Value> {
        let owned;
        let p: &str = match &args[0] {
            Value::Text(t) => t.as_str(),
            Value::Null => return Ok(Value::Null),
            other => {
                owned = other.to_string();
                owned.as_str()
            }
        };
        let md = match std::fs::metadata(p) {
            Ok(m) => m,
            Err(_) => return Ok(Value::Null),
        };
        if md.is_dir() {
            // SQLite's readfile on a directory: the fstat's size feeds
            // the read loop, which never terminates against a
            // directory fd and hits the length cap — surfaced as
            // "string or blob too big" (oracle-pinned).
            return Err(Error::semantic("string or blob too big"));
        }
        match std::fs::read(p) {
            Ok(bytes) => Ok(Value::Blob(bytes.into_boxed_slice().into())),
            Err(_) => Ok(Value::Null),
        }
    }
}

struct WriteFileFn;
impl ScalarFunction for WriteFileFn {
    fn name(&self) -> &str {
        "writefile"
    }
    fn arity(&self) -> Arity {
        Arity::Variadic
    }
    fn call(&self, _ctx: &FnCtx, args: &[Value]) -> Result<Value> {
        let owned;
        let p: &str = match &args[0] {
            Value::Text(t) => t.as_str(),
            Value::Null => return Ok(Value::Null),
            other => {
                owned = other.to_string();
                owned.as_str()
            }
        };
        let bytes: Vec<u8> = match args.get(1) {
            Some(Value::Blob(b)) => b.clone(),
            Some(Value::Text(t)) => t.as_bytes().to_vec(),
            Some(Value::Null) | None => Vec::new(),
            Some(other) => other.to_string().into_bytes(),
        };
        // The shell's writefile CREATES missing parent directories
        // (fileio.c's mkdir-p discipline — the oracle's .ar -x relies
        // on it for archive members like `sub/c.txt`; pinned against
        // the real 3.53.4 binary: writefile('/x/nodir/f', ...) → 3).
        if let Some(parent) = std::path::Path::new(p).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        if std::fs::write(p, &bytes).is_err() {
            return Ok(Value::Null);
        }
        if let Some(m) = args.get(2) {
            if let Some(mode) = Some(m.as_integer()) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        p,
                        std::fs::Permissions::from_mode(mode as u32 & 0o7777),
                    );
                }
                #[cfg(not(unix))]
                {
                    let _ = mode;
                }
            }
        }
        if let Some(t) = args.get(3) {
            if let Some(mtime) = Some(t.as_integer()) {
                set_mtime(p, mtime);
            }
        }
        Ok(Value::Integer(bytes.len() as i64))
    }
}

fn set_mtime(p: &str, mtime: i64) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let cpath = std::ffi::CString::new(std::ffi::OsStr::from_bytes(p.as_bytes()).as_bytes());
        let times: [libc::timespec; 2] = [
            libc::timespec {
                tv_sec: mtime,
                tv_nsec: 0,
            },
            libc::timespec {
                tv_sec: mtime,
                tv_nsec: 0,
            },
        ];
        if let Ok(cp) = cpath {
            unsafe {
                let _ = libc::utimensat(libc::AT_FDCWD, cp.as_ptr(), times.as_ptr(), 0);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (p, mtime);
    }
}

// ============================================================
// shell_putsnl

struct ShellPutsNlFn;
impl ScalarFunction for ShellPutsNlFn {
    fn name(&self) -> &str {
        "shell_putsnl"
    }
    fn arity(&self) -> Arity {
        Arity::Exact(1)
    }
    fn call(&self, _ctx: &FnCtx, args: &[Value]) -> Result<Value> {
        let s = match &args[0] {
            Value::Text(t) => t.as_str().to_string(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        puts_line(&s);
        Ok(args[0].clone())
    }
}

// ============================================================
// sqlar_compress / sqlar_uncompress

struct SqlarCompressFn;
impl ScalarFunction for SqlarCompressFn {
    fn name(&self) -> &str {
        "sqlar_compress"
    }
    fn arity(&self) -> Arity {
        Arity::Exact(1)
    }
    fn deterministic(&self) -> bool {
        true
    }
    fn call(&self, _ctx: &FnCtx, args: &[Value]) -> Result<Value> {
        // The no-zlib discipline: members are stored raw (see the
        // module docs) — compression is the identity.
        Ok(args[0].clone())
    }
}

struct SqlarUncompressFn;
impl ScalarFunction for SqlarUncompressFn {
    fn name(&self) -> &str {
        "sqlar_uncompress"
    }
    fn arity(&self) -> Arity {
        Arity::Exact(2)
    }
    fn deterministic(&self) -> bool {
        true
    }
    fn call(&self, _ctx: &FnCtx, args: &[Value]) -> Result<Value> {
        let owned;
        let data: &[u8] = match &args[0] {
            Value::Blob(b) => b.as_ref(),
            Value::Text(t) => t.as_bytes(),
            Value::Null => return Ok(Value::Null),
            other => {
                owned = other.to_string();
                owned.as_bytes()
            }
        };
        let sz = args[1].as_integer().max(0) as usize;
        // sqlar's convention: same length = stored raw.
        if data.len() == sz {
            return Ok(args[0].clone());
        }
        if data.is_empty() {
            return Ok(Value::Blob(Vec::new().into_boxed_slice().into()));
        }
        let out = crate::plugin::zipfile::inflate_zlib(data, sz)?;
        Ok(Value::Blob(out.into_boxed_slice().into()))
    }
}

/// Public lsmode for the CLI's archive listing (ar.c prints the
/// mode column through the same function the SQL surface exposes).
pub fn ls_mode_pub(mode: i64) -> String {
    ls_mode(mode)
}

/// Register the family on a database handle.
pub fn register(db: &mut crate::Database) -> Result<()> {
    db.create_function(LsModeFn)?;
    db.create_function(RealPathFn)?;
    db.create_function(ReadFileFn)?;
    db.create_function(WriteFileFn)?;
    db.create_function(ShellPutsNlFn)?;
    db.create_function(SqlarCompressFn)?;
    db.create_function(SqlarUncompressFn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsmode_goldens() {
        assert_eq!(ls_mode(33188), "-rw-r--r--");
        assert_eq!(ls_mode(16893), "drwxrwxr-x");
        assert_eq!(ls_mode(41471), "lrwxrwxrwx");
        assert_eq!(ls_mode(33204), "-rw-rw-r--");
        assert_eq!(ls_mode(16832), "drwx------");
        assert_eq!(ls_mode(8630), "?rw-rw-rw-");
        assert_eq!(ls_mode(0), "?---------");
        assert_eq!(ls_mode(0o100000), "----------");
        assert_eq!(ls_mode(0o100777), "-rwxrwxrwx");
        // The shell's lsmode(NULL) and lsmode(0) renderings.
        assert_eq!(
            LsModeFn.call(&FnCtx::new(1), &[Value::Null]).unwrap(),
            Value::Text("?---------".into())
        );
    }

    #[test]
    fn realpath_shapes() {
        // Lexical fallback (the file may not exist); separator-agnostic
        // so the same contract holds on windows path rendering.
        let r = real_path("/x/../y");
        let last = r.rsplit(['/', '\\']).next().unwrap_or("");
        assert_eq!(last, "y", "realpath('/x/../y') = {r}");
        assert!(!r.contains(".."));
        let here = real_path(".");
        assert!(!here.is_empty());
        assert!(std::path::Path::new(&here).is_absolute());
    }

    #[test]
    fn writefile_readfile_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rustqlite-wf-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("wf.bin");
        let ps = p.to_str().unwrap();
        let n = WriteFileFn
            .call(
                &FnCtx::new(2),
                &[
                    Value::Text(ps.into()),
                    Value::Blob(vec![1u8, 2, 3].into_boxed_slice().into()),
                ],
            )
            .unwrap();
        assert_eq!(n, Value::Integer(3));
        let back = ReadFileFn
            .call(&FnCtx::new(1), &[Value::Text(ps.into())])
            .unwrap();
        assert_eq!(back, Value::Blob(vec![1u8, 2, 3].into_boxed_slice().into()));
        let _ = std::fs::remove_file(&p);
        // Missing file → NULL.
        let none = ReadFileFn
            .call(&FnCtx::new(1), &[Value::Text(ps.into())])
            .unwrap();
        assert!(none.is_null());
    }
}
