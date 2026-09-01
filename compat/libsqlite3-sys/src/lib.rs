//! # rustqlite sys-crate (drop-in `libsqlite3-sys` replacement)
//!
//! This is a patched copy of `libsqlite3-sys` 0.30.1 (MIT, upstream:
//! https://github.com/rusqlite/rusqlite) whose build script links the
//! **rustqlite engine's SQLite C ABI compatibility library**
//! (`rustqlite-compat`, exporting the real `sqlite3_*` symbols) instead of
//! the C SQLite library. Use it via:
//!
//! ```toml
//! [patch.crates-io]
//! libsqlite3-sys = { path = "path/to/rust-sql/compat/libsqlite3-sys" }
//! ```
//!
//! The Rust-facing surface (types, constants, `SQLITE_STATIC` /
//! `SQLITE_TRANSIENT`, error codes) is identical to upstream, so `sqlx`,
//! `rusqlite`-style code and everything in between compile unmodified.
//
// Upstream: libsqlite3-sys 0.30.1 — Copyright (c) Rusqlite contributors
// (MIT). Bindings file: bindgen-bindings/bindgen_3.14.0.rs, generated
// upstream from sqlite3.h 3.14+; the constants and signatures match the
// sqlite3.h the rustqlite-compat library implements.
#![allow(non_snake_case, non_camel_case_types)]
#![cfg_attr(test, allow(deref_nullptr))] // https://github.com/rust-lang/rust-bindgen/issues/2066

// force linking to openssl
#[cfg(feature = "bundled-sqlcipher-vendored-openssl")]
extern crate openssl_sys;

pub use self::error::*;

use std::mem;

mod error;

#[must_use]
pub fn SQLITE_STATIC() -> sqlite3_destructor_type {
    None
}

#[must_use]
pub fn SQLITE_TRANSIENT() -> sqlite3_destructor_type {
    Some(unsafe { mem::transmute::<isize, unsafe extern "C" fn(*mut std::ffi::c_void)>(-1_isize) })
}

#[allow(dead_code, clippy::all)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindgen.rs"));
}
pub use bindings::*;

impl Default for sqlite3_vtab {
    fn default() -> Self {
        unsafe { mem::zeroed() }
    }
}

impl Default for sqlite3_vtab_cursor {
    fn default() -> Self {
        unsafe { mem::zeroed() }
    }
}
