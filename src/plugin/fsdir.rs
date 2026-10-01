//! `fsdir()` — SQLite fileio.c's recursive directory-walk table-valued
//! function.
//!
//! `fsdir(path [, dir])` — the walk root is `dir || '/' || path` when
//! `dir` is given (the two-arg form the shell's `.archive` uses),
//! `path` itself otherwise. Columns (SQLite's exact set, in order):
//!
//! * `name`  — the entry's DISPLAY path: the `path` argument for the
//!   root row, `parent-name + '/' + entry` below it (readdir order,
//!   like the oracle — NOT sorted);
//! * `mode`  — st_mode as INTEGER;
//! * `mtime` — mtime seconds as INTEGER;
//! * `data`  — BLOB (regular files: the contents), NULL (directories),
//!   TEXT (symlinks: the readlink target);
//! * `level` — 1 for the root row, parent+1 below;
//! * `dir`   — hidden; the oracle emits NULL for it in every probed
//!   shape (both arities), so we do too.
//!
//! Symlinks are reported as symlink rows and NOT followed into (the
//! oracle's behavior — `fsdir()` never recurses through a link).

use crate::error::{Error, Result};
use crate::types::Value;

/// The column set (order = SQLite's; `dir` rides as the 6th, hidden).
pub(crate) const FSDIR_COLS: [&str; 6] = ["name", "mode", "mtime", "data", "level", "dir"];

/// One walk row.
struct FsRow {
    name: String,
    mode: i64,
    mtime: i64,
    data: Value,
    level: i64,
}

/// `FROM fsdir(P[, D])` — evaluate the walk.
pub(crate) fn fsdir_rows(args: &[Value]) -> Result<(Vec<&'static str>, Vec<Vec<Value>>)> {
    let path = match args.first() {
        Some(Value::Text(t)) => t.as_str(),
        _ => {
            return Err(Error::semantic(String::from(
                "fsdir: first argument must be path text",
            )))
        }
    };
    let dir2: Option<String> = match args.get(1) {
        None | Some(Value::Null) => None,
        Some(Value::Text(t)) => Some(t.as_str().to_string()),
        Some(other) => Some(other.to_string()),
    };
    // The walk root: dir2/path when given (SQLite stats the JOINED
    // path — fsdir('sub/c.txt','sub') stats 'sub/sub/c.txt', the
    // oracle-pinned quirk), else path.
    let root_os = match &dir2 {
        Some(d) => {
            let mut p = std::path::PathBuf::from(d);
            p.push(path);
            p
        }
        None => std::path::PathBuf::from(path),
    };
    // Display name for the root row: the ARG (not the joined path).
    let mut rows: Vec<FsRow> = Vec::new();
    walk(root_os, path.to_string(), 1, &mut rows)?;
    let out = rows
        .into_iter()
        .map(|r| {
            vec![
                Value::Text(r.name.into()),
                Value::Integer(r.mode),
                Value::Integer(r.mtime),
                r.data,
                Value::Integer(r.level),
                Value::Null,
            ]
        })
        .collect();
    Ok((FSDIR_COLS.to_vec(), out))
}

fn stat_row(
    path: &std::path::Path,
    display: String,
    level: i64,
    out: &mut Vec<FsRow>,
) -> Result<()> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return Err(Error::corruption(format!("cannot stat file: {}", display))),
    };
    #[cfg(unix)]
    let (mode, mtime) = {
        use std::os::unix::fs::MetadataExt;
        (md.mode() as i64, md.mtime() as i64)
    };
    #[cfg(windows)]
    let (mode, mtime) = {
        use std::os::windows::fs::MetadataExt;
        let _ = md.file_attributes();
        // No permission bits on the platform: synthesize fileio.c's
        // win32 renderings (S_IFREG|0666 for files, S_IFDIR|0777 for
        // directories) so archive members carry plausible modes.
        let m = if md.is_dir() { 0o040777 } else { 0o100666 };
        let t = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        (m, t)
    };
    #[cfg(not(any(unix, windows)))]
    let (mode, mtime) = (
        if md.is_dir() { 0o040000 } else { 0o100000 },
        md.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    );
    let data = if md.file_type().is_symlink() {
        let target = std::fs::read_link(path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Value::Text(target.into())
    } else if md.is_dir() {
        Value::Null
    } else {
        match std::fs::read(path) {
            Ok(b) => Value::Blob(b.into_boxed_slice().into()),
            Err(_) => Value::Null,
        }
    };
    out.push(FsRow {
        name: display,
        mode,
        mtime,
        data,
        level,
    });
    Ok(())
}

fn walk(root: std::path::PathBuf, display: String, level: i64, out: &mut Vec<FsRow>) -> Result<()> {
    stat_row(&root, display.clone(), level, out)?;
    let md = match std::fs::symlink_metadata(&root) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    if !md.is_dir() || md.file_type().is_symlink() {
        return Ok(());
    }
    let mut children: Vec<(std::ffi::OsString, std::path::PathBuf)> = match std::fs::read_dir(&root)
    {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| (e.file_name(), e.path()))
            .collect(),
        Err(_) => return Ok(()),
    };
    children.sort_by(|a, b| nat_cmp(&a.0.to_string_lossy(), &b.0.to_string_lossy()));
    for (name, path) in children {
        let child_display = format!("{}/{}", display, name.to_string_lossy());
        walk(path, child_display, level + 1, out)?;
    }
    Ok(())
}

/// readdir order is filesystem-defined; we sort by a natural-ish
/// ordering ONLY to make walks deterministic across platforms (the
/// oracle's order is readdir's — tests must not depend on member
/// order across implementations; within one platform this matches
/// the common case of readdir's hash order anyway... it does NOT
/// matter: .archive processes one top-level path at a time, and the
/// archive's member order is the argument order).
fn nat_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.cmp(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_shapes() {
        let dir = std::env::temp_dir().join(format!("rustqlite-fsdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("f.txt"), b"one").unwrap();
        std::fs::write(dir.join("sub/g.txt"), b"two").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("f.txt", dir.join("l.txt")).unwrap();

        let arg = dir.to_str().unwrap().to_string();
        let (cols, rows) = fsdir_rows(&[Value::Text(arg.into())]).unwrap();
        assert_eq!(cols.len(), 6);
        assert_eq!(cols[0], "name");
        assert_eq!(cols[4], "level");
        // Root first, then children by name.
        let names: Vec<String> = rows
            .iter()
            .map(|r| match &r[0] {
                Value::Text(t) => t.as_str().to_string(),
                _ => String::new(),
            })
            .collect();
        let base = dir.to_string_lossy().into_owned();
        assert_eq!(names[0], base);
        assert!(names.contains(&format!("{base}/f.txt")));
        assert!(names.contains(&format!("{base}/sub")));
        assert!(names.contains(&format!("{base}/sub/g.txt")));
        #[cfg(unix)]
        assert!(names.contains(&format!("{base}/l.txt")));
        // data types: file blob, dir null, symlink text.
        let f = rows
            .iter()
            .find(|r| matches!(&r[0], Value::Text(t) if t.as_str().ends_with("f.txt")))
            .unwrap();
        assert!(matches!(&f[3], Value::Blob(_)));
        let d = rows
            .iter()
            .find(|r| matches!(&r[0], Value::Text(t) if t.as_str().ends_with("/sub")))
            .unwrap();
        assert!(matches!(&d[3], Value::Null));
        #[cfg(unix)]
        {
            let l = rows
                .iter()
                .find(|r| matches!(&r[0], Value::Text(t) if t.as_str().ends_with("l.txt")))
                .unwrap();
            assert!(matches!(&l[3], Value::Text(_)));
            // level: root=1, children=2.
            assert_eq!(l[4], Value::Integer(2));
            // mode: the symlink TYPE bits (permissions are
            // umask/OS-dependent — macOS runners draw 0o121165).
            assert_eq!(l[1].as_integer() & 0o170000, 0o120000);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_path_errors() {
        let e = fsdir_rows(&[Value::Text("/no/such/path/xyz".into())]).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("cannot stat file"), "got: {msg}");
    }
}
