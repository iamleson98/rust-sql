//! Build script for the rustqlite-backed `libsqlite3-sys` replacement.
//!
//! Two link modes, selected with the `RUSTQLITE_LINK_MODE` env var:
//!
//! - `dylib` (default): links the `rustqlite-compat` C ABI library
//!   (`libsqlite3.so` — real `sqlite3_*` symbols implemented on the
//!   rustqlite engine) and bakes an rpath to it. Library location from
//!   `RUSTQLITE_LIB_DIR` or the repo-relative default
//!   `<repo>/target/release`.
//!   NOTE: build-script link *args* (rpath) do not propagate through rlib
//!   metadata to the final binary — consumers that cannot control the
//!   final link line should prefer `rlib` mode.
//! - `rlib`: emits NO native link flags at all. The consumer adds
//!   `rustqlite-compat` as a Rust path dependency; its `#[no_mangle]`
//!   `sqlite3_*` exports are compiled into the final binary and satisfy
//!   the undefined references from sqlx-sqlite / rusqlite-style code at
//!   the final link. Fully static single binary: no libsqlite3.so, no
//!   rpath, no accidental fallback to a system libsqlite3.
//!
//! In both modes the vendored bindings are copied to OUT_DIR.

use std::env;
use std::path::PathBuf;

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
        // backend's `src/db/sqlite_migrate.rs` link anchor. Do NOT try to
        // substitute the staticlib artifact here via rustc-link-lib: the
        // bundled engine members would fight the root crate's own
        // #[global_allocator] shims (duplicate __rust_alloc).
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

    println!(
        "cargo:rerun-if-changed={}",
        lib_dir.join("libsqlite3.so").display()
    );
    println!("cargo:rustc-link-lib=dylib=sqlite3");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    // Runtime resolution without LD_LIBRARY_PATH: embed an rpath.
    // (Kept even for static link scenarios — the linker drops unused args.)
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
    // Expose the lib dir to dependents (e.g. for embedding checks).
    println!("cargo:root={}", manifest_dir.display());
}
