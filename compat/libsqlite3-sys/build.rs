//! Build script for the rustqlite-backed `libsqlite3-sys` replacement.
//!
//! Two link modes, selected with the `RUSTQLITE_LINK_MODE` env var:
//!
//! - `dylib` (default): links the `rustqlite-compat` C ABI library
//!   (`libsqlite3.so` on Linux, `libsqlite3.dylib` on macOS, `sqlite3.dll`
//!   on Windows — real `sqlite3_*` symbols implemented on the rustqlite
//!   engine) and bakes an rpath to it (Unix only; Windows has no rpath).
//!   Library location from `RUSTQLITE_LIB_DIR` or the repo-relative default
//!   `<repo>/target/release`.
//!   NOTE: build-script link *args* (rpath) do not propagate through rlib
//!   metadata to the final binary — consumers that cannot control the
//!   final link line should prefer `rlib` mode.
//! - `rlib`: emits NO native link flags at all. The consumer adds
//!   `rustqlite-compat` as a Rust path dependency; its `#[no_mangle]`
//!   `sqlite3_*` exports are compiled into the final binary and satisfy
//!   the undefined references from sqlx-sqlite / rusqlite-style code at
//!   the final link. Fully static single binary: no shared engine
//!   library, no rpath, no accidental fallback to a system libsqlite3.
//!
//! In both modes the vendored bindings are copied to OUT_DIR.
//!
//! # Cross-platform engine artifacts (crate name `sqlite3`, crate-type
//! cdylib)
//!
//! | target            | shared library      | import library          |
//! |-------------------|----------------------|-------------------------|
//! | x86_64/linux      | `libsqlite3.so`      | —                       |
//! | aarch64/darwin    | `libsqlite3.dylib`   | —                       |
//! | windows-msvc      | `sqlite3.dll`        | `sqlite3.dll.lib` (in `target/release/deps/`) |
//! | windows-gnu       | `sqlite3.dll`        | `libsqlite3.dll.a` (in `target/release/deps/`) |
//!
//! Unix linking uses `-L <lib_dir> -l <link_name>`; the optional alias
//! file (`lib<link_name>.so` / `.dylib`) is materialized here as an
//! mtime-guarded copy of the canonical artifact. Windows-MSVC linking
//! needs an import library named exactly `<link_name>.lib` on the `-L`
//! search path (link.exe matches the literal name; it does not try
//! `<name>.dll.lib`), so the engine's import library is copied to that
//! name. The DLL name baked into the import stubs stays `sqlite3.dll`,
//! so the runtime loader looks for `sqlite3.dll` — this script copies it
//! next to the profile's binaries (exe dir + `deps/`) where the Windows
//! loader finds it: `cargo run` executables search their own directory
//! first, and rustc loads proc-macro cdylibs (sqlx-macros) with
//! `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR`, which searches the importing
//! DLL's directory (`deps/`).
//!
//! Build the engine first — from the consumer's `backend/` directory:
//! `make engine`, or directly:
//! `cargo build --release -p rustqlite-compat --manifest-path vendor/rust-sql/Cargo.toml`.

use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // 1. Copy the vendored (upstream-generated) bindings into OUT_DIR.
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bindings_src = manifest_dir
        .join("bindgen-bindings")
        .join("bindgen_3.14.0.rs");
    std::fs::copy(&bindings_src, out_dir.join("bindgen.rs"))
        .expect("vendored bindings file missing (bindgen-bindings/bindgen_3.14.0.rs)");
    println!("cargo:rerun-if-changed={}", bindings_src.display());

    // 2. Select the link mode.
    println!("cargo:rerun-if-env-changed=RUSTQLITE_LINK_MODE");
    let mode = env::var("RUSTQLITE_LINK_MODE").unwrap_or_else(|_| "dylib".to_owned());
    if mode == "rlib" {
        // The consumer links `rustqlite-compat` as a Rust path dependency;
        // the crate's `sqlite3_*` #[no_mangle] exports are compiled into
        // every binary that keeps the rlib on its link line.
        //
        // NOTE the line-selection rule this relies on: rustc keeps an
        // rlib on a final binary's link line only when the ROOT crate's
        // compilation actually references the crate. Rust-visible C ABI
        // exports alone are invisible to that analysis, so the compat
        // crate exposes a small Rust API (`engine_version`) for consumers
        // to reference from real code — see `engine_link` below and the
        // backend's `src/infrastructure/db_engine.rs` link anchor. Do NOT
        // try to substitute the staticlib artifact here via
        // rustc-link-lib: the bundled engine members would fight the root
        // crate's own #[global_allocator] shims (duplicate __rust_alloc).
        return;
    }

    // 3. dylib mode: locate the rustqlite compat C ABI library.
    println!("cargo:rerun-if-env-changed=RUSTQLITE_LIB_DIR");
    let lib_dir = env::var("RUSTQLITE_LIB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            // compat/libsqlite3-sys -> repo root -> target/release
            manifest_dir
                .join("..")
                .join("..")
                .join("target")
                .join("release")
                .canonicalize()
                .unwrap_or_else(|_| {
                    manifest_dir
                        .join("..")
                        .join("..")
                        .join("target")
                        .join("release")
                })
        });

    // Platform-specific artifact naming (see the module docs table).
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let is_windows = target_os == "windows";
    let (dylib_name, dylib_ext) = match target_os.as_str() {
        "windows" => ("sqlite3.dll", "dll"),
        "macos" => ("libsqlite3.dylib", "dylib"),
        _ => ("libsqlite3.so", "so"),
    };
    let canonical = lib_dir.join(dylib_name);
    println!("cargo:rerun-if-changed={}", canonical.display());

    // Optional unique link name (`RUSTQLITE_LINK_NAME`, default `sqlite3`).
    //
    // Motivation: machines with libsqlite3-dev installed expose a REAL
    // system `libsqlite3.so`, and another dependency's build script can
    // put the system lib dir ahead of RUSTQLITE_LIB_DIR on the -L search
    // order — `-lsqlite3` then silently binds the SYSTEM SQLite, which
    // satisfies every stock sqlite3_* symbol (only consumers calling
    // engine-specific extensions notice). Linking under a distinctive
    // name no system library carries removes the ambiguity at build
    // time AND at runtime: the compat cdylib sets no SONAME, so the
    // linker records the matched filename as DT_NEEDED — an alias name
    // makes the binary load the rustqlite engine, and only it, wherever
    // the loader finds that name (LD_LIBRARY_PATH / rpath / ldconfig).
    //
    // The alias file is materialized here (mtime-guarded copy of the
    // canonical artifact), so a plain rebuild of the engine is
    // enough — no extra recipe step to forget.
    println!("cargo:rerun-if-env-changed=RUSTQLITE_LINK_NAME");
    let link_name = env::var("RUSTQLITE_LINK_NAME").unwrap_or_else(|_| "sqlite3".to_owned());
    if link_name != "sqlite3" {
        if is_windows {
            alias_windows_import_lib(&lib_dir, &link_name, &target_env);
        } else {
            // Unix alias: libsqlite3.so -> lib<link_name>.so (or .dylib).
            let alias = lib_dir.join(format!("lib{}.{}", link_name, dylib_ext));
            copy_alias(&canonical, &alias, &link_name);
        }
    }

    // Fail fast with actionable instructions when the engine was never
    // built — otherwise the failure surfaces later as an opaque linker
    // "cannot find -l<name>" error.
    assert!(
        canonical.exists(),
        "rustqlite engine library not found: {}. Build it first — from the \
         backend/ directory run `make engine` (or: cargo build --release \
         -p rustqlite-compat --manifest-path vendor/rust-sql/Cargo.toml).",
        canonical.display()
    );

    println!("cargo:rustc-link-lib=dylib={}", link_name);
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    if is_windows {
        // No rpath on Windows: make the engine DLL loadable by the
        // artifacts this build produces instead (exe dir + deps dir).
        copy_engine_dll_for_runtime(&lib_dir, &canonical);
    } else {
        // Runtime resolution without LD_LIBRARY_PATH: embed an rpath.
        // (Kept even for static link scenarios — the linker drops unused
        // args.)
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
    }
    // Expose the lib dir to dependents (e.g. for embedding checks).
    println!("cargo:root={}", manifest_dir.display());
}

/// mtime-guarded copy used for every alias the script materializes:
/// copy when the destination is missing or older than the source.
fn copy_alias(src: &Path, dst: &Path, link_name: &str) {
    let needs_copy = match (std::fs::metadata(src), std::fs::metadata(dst)) {
        (Ok(s), Ok(d)) => match (s.modified(), d.modified()) {
            (Ok(a), Ok(b)) => a > b,
            _ => true,
        },
        _ => true,
    };
    if needs_copy {
        std::fs::copy(src, dst).unwrap_or_else(|e| {
            panic!(
                "RUSTQLITE_LINK_NAME={} requested, but the rustqlite engine \
                 library {} could not be aliased to {}: {}. Build the engine \
                 first — from the backend/ directory run `make engine` (or: \
                 cargo build --release -p rustqlite-compat --manifest-path \
                 vendor/rust-sql/Cargo.toml).",
                link_name,
                src.display(),
                dst.display(),
                e
            )
        });
        println!("cargo:rerun-if-changed={}", dst.display());
    }
}

/// Windows: create `<link_name>.lib` (msvc) / `lib<link_name>.dll.a` (gnu)
/// on the -L search path from the engine's import library.
///
/// MSVC's link.exe resolves `-l<name>` to the literal `<name>.lib`; the
/// engine build produces `sqlite3.dll.lib` (cargo leaves the import
/// library in `target/release/deps/`, only the DLL is copied up to
/// `target/release/`). GNU ld's search order tries
/// `lib<name>.dll.a` first. The import stubs keep referencing
/// `sqlite3.dll`, which is exactly what the runtime loader must find.
fn alias_windows_import_lib(lib_dir: &Path, link_name: &str, target_env: &str) {
    let gnu = target_env == "gnu";
    let candidates: Vec<PathBuf> = if gnu {
        vec![
            lib_dir.join("libsqlite3.dll.a"),
            lib_dir.join("deps").join("libsqlite3.dll.a"),
        ]
    } else {
        vec![
            lib_dir.join("sqlite3.dll.lib"),
            lib_dir.join("deps").join("sqlite3.dll.lib"),
            lib_dir.join("libsqlite3.dll.lib"),
            lib_dir.join("deps").join("libsqlite3.dll.lib"),
        ]
    };
    let alias = if gnu {
        lib_dir.join(format!("lib{}.dll.a", link_name))
    } else {
        lib_dir.join(format!("{}.lib", link_name))
    };
    match candidates.iter().find(|c| c.exists()) {
        Some(src) => copy_alias(src, &alias, link_name),
        None => panic!(
            "RUSTQLITE_LINK_NAME={} requested on Windows, but the engine's \
             import library was not found (looked for {}). Build the engine \
             first — from the backend/ directory run `make engine` (or: cargo \
             build --release -p rustqlite-compat --manifest-path \
             vendor/rust-sql/Cargo.toml), which produces sqlite3.dll plus \
             its import library.",
            link_name,
            candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Windows runtime resolution: the import tables reference `sqlite3.dll`,
/// and the Windows loader searches (1) the importing executable's own
/// directory, then system dirs / PATH. Copy the DLL to the profile dir
/// (where `cargo run` puts executables) and to `deps/` (where test
/// binaries AND the sqlx proc-macro cdylibs live — rustc dlopens those
/// with LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, which searches the importing
/// DLL's own directory).
///
/// OUT_DIR is `target/<profile>/build/libsqlite3-sys-<hash>/out`, so the
/// profile dir is three levels up. The copy is mtime-guarded and shared
/// by every compilation unit that runs this script (same target dir).
fn copy_engine_dll_for_runtime(_lib_dir: &Path, canonical: &Path) {
    let Ok(out_dir) = env::var("OUT_DIR") else {
        return;
    };
    let out_dir = PathBuf::from(out_dir);
    // out -> libsqlite3-sys-<hash> -> build -> <profile>
    let Some(profile_dir) = out_dir.ancestors().nth(3) else {
        return;
    };
    for dest in [profile_dir.to_path_buf(), profile_dir.join("deps")] {
        let target = dest.join(canonical.file_name().unwrap_or_default());
        copy_alias(canonical, &target, "sqlite3-runtime");
    }
}
