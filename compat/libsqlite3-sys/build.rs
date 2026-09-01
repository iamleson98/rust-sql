//! Build script for the rustqlite-backed `libsqlite3-sys` replacement.
//!
//! Links the `rustqlite-compat` C ABI library (`libsqlite3.so` — real
//! `sqlite3_*` symbols implemented on the rustqlite engine) and bakes an
//! rpath to it so the final binary finds it at runtime with zero env vars.
//!
//! Library location resolution order:
//!   1. `RUSTQLITE_LIB_DIR` env var (absolute path containing
//!      libsqlite3.so)
//!   2. `<repo>/target/release` — the output of
//!      `cargo build --release -p rustqlite-compat` in the rust-sql
//!      workspace this crate lives in.

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // 1. Copy the vendored (upstream-generated) bindings into OUT_DIR.
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bindings_src = manifest_dir.join("bindgen-bindings").join("bindgen_3.14.0.rs");
    std::fs::copy(&bindings_src, out_dir.join("bindgen.rs")).expect(
        "vendored bindings file missing (bindgen-bindings/bindgen_3.14.0.rs)",
    );
    println!("cargo:rerun-if-changed={}", bindings_src.display());

    // 2. Locate the rustqlite compat C ABI library.
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

    println!("cargo:rerun-if-env-changed=RUSTQLITE_LIB_DIR");
    println!("cargo:rerun-if-changed={}", lib_dir.join("libsqlite3.so").display());
    println!("cargo:rustc-link-lib=dylib=sqlite3");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    // Runtime resolution without LD_LIBRARY_PATH: embed an rpath.
    // (Kept even for static link scenarios — the linker drops unused args.)
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
    // Expose the lib dir to dependents (e.g. for embedding checks).
    println!("cargo:root={}", manifest_dir.display());
}
