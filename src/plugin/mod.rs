//! Plugin system: user-defined scalar functions, aggregate functions,
//! collations, virtual-table modules, and page codecs.
//!
//! This is the Rust-side ("static extension") API. It mirrors SQLite's
//! runtime extension surface (`sqlite3_create_function`,
//! `sqlite3_create_collation`, `sqlite3_create_module`) but with safe Rust
//! traits. A C-ABI layer ([`crate::ffi`], [`crate::plugin::abi`]) adapts the
//! same registry so extensions written in C, C++, Zig, or Rust can be
//! compiled to shared libraries and loaded at runtime with
//! [`Database::load_extension`](crate::api::Database::load_extension).
//!
//! # Dispatch model
//!
//! The registry lives on [`Database`] behind an `Arc` snapshot. At statement
//! start, the engine installs the snapshot into a thread-local scope (see
//! [`scope`]) — exactly like the correlated-subquery bridge (`CorrGuard`),
//! so deeply nested evaluator code (`call_scalar`, aggregate resolution,
//! collation compare) can reach it without threading a reference through
//! every signature. Lookups pay ONE thread-local read; built-in functions
//! are matched first, so the registry is only consulted for unknown names.

use crate::error::{Error, Result};
use crate::types::Value;
use std::collections::HashMap;
use std::cmp::Ordering;
use std::sync::Arc;

pub mod abi;
pub mod codec;
pub mod vtab;

pub use abi::{CAggregate, CCollation, CScalar};
pub use codec::PageCodec;
pub use vtab::{
    IndexInfo, VtabConstraint, VtabUpdateArg, VirtualTable, VirtualTableCursor, VirtualTableModule,
};

/// Argument-count policy for a user function, mirroring SQLite's `nArg`
/// parameter in `sqlite3_create_function` (`-1` means variadic).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arity {
    Exact(usize),
    Variadic,
}

impl Arity {
    pub fn accepts(&self, n: usize) -> bool {
        match *self {
            Arity::Exact(k) => n == k,
            Arity::Variadic => true,
        }
    }
}

/// Context passed to a scalar function call.
///
/// Currently carries call metadata; the C-ABI layer extends this with a
/// result slot + error slot so `xFunc` trampolines can write outputs.
pub struct FnCtx {
    /// Argument count declared at registration (for variadic functions
    /// that want to know the exact call arity — also exposed as
    /// `args.len()`, so most implementations can ignore this).
    pub argc: usize,
    /// The `pApp` pointer registered with the C ABI
    /// (`sqlite3_create_function`'s fourth argument). Always null for
    /// pure-Rust registrations.
    pub app: *mut std::ffi::c_void,
}

impl FnCtx {
    pub fn new(argc: usize) -> Self {
        Self { argc, app: std::ptr::null_mut() }
    }
}

// SAFETY: `app` is an opaque user pointer; the C ABI contract requires the
// plugin to use it only from the calling thread (SQLite has the same rule).
unsafe impl Send for FnCtx {}
unsafe impl Send for AggCtx {}

/// A user-defined scalar function.
pub trait ScalarFunction: Send + Sync {
    /// SQL name (case-insensitive at call sites).
    fn name(&self) -> &str;
    /// Declared argument count.
    fn arity(&self) -> Arity {
        Arity::Variadic
    }
    /// Whether repeated calls with identical arguments return identical
    /// results (allows constant folding in future planners).
    fn deterministic(&self) -> bool {
        false
    }
    /// Invoke the function.
    fn call(&self, ctx: &FnCtx, args: &[Value]) -> Result<Value>;
}

/// Per-group state of a user-defined aggregate function.
pub trait AggState: Send {
    /// Called once per input row.
    fn step(&mut self, ctx: &AggCtx, args: &[Value]) -> Result<()>;
    /// Called once per group at the end. Consumes nothing (the state is
    /// dropped afterwards).
    fn value(&self) -> Result<Value>;
}

/// Context passed to aggregate step calls (mirrors `FnCtx`).
pub struct AggCtx {
    pub argc: usize,
    pub app: *mut std::ffi::c_void,
}

impl AggCtx {
    pub fn new(argc: usize) -> Self {
        Self { argc, app: std::ptr::null_mut() }
    }
}

/// A user-defined aggregate function.
pub trait AggregateFunction: Send + Sync {
    fn name(&self) -> &str;
    fn arity(&self) -> Arity {
        Arity::Variadic
    }
    /// Create one state object per group.
    fn init(&self) -> Box<dyn AggState>;
}

/// A user-defined collation sequence.
///
/// Applies to TEXT comparison (SQLite semantics: collations only affect
/// text; numbers keep their total order). Used by ORDER BY ... COLLATE,
/// comparison operators with a COLLATE operand, GROUP BY, and DISTINCT.
pub trait Collation: Send + Sync {
    /// Collation name (SQL: `COLLATE name`).
    fn name(&self) -> &str;
    /// Compare two TEXT values through the collation.
    fn compare(&self, a: &str, b: &str) -> Ordering;
}

// ---------------------------------------------------------------------------
// Built-in collations: NOCASE / RTRIM (BINARY is Rust's default Value order)
// ---------------------------------------------------------------------------

/// `NOCASE`: ASCII case-insensitive comparison (SQLite's built-in NOCASE is
/// ASCII-only; it does NOT fold non-ASCII bytes).
pub struct NoCaseCollation;

impl Collation for NoCaseCollation {
    fn name(&self) -> &str {
        "NOCASE"
    }
    fn compare(&self, a: &str, b: &str) -> Ordering {
        // Compare as bytes with ASCII folding, so multi-byte UTF-8 keeps
        // byte ordering (matching SQLite, which compares raw bytes).
        let (a, b) = (a.as_bytes(), b.as_bytes());
        let n = a.len().min(b.len());
        for i in 0..n {
            let ca = a[i].to_ascii_lowercase();
            let cb = b[i].to_ascii_lowercase();
            match ca.cmp(&cb) {
                Ordering::Equal => continue,
                ord => return ord,
            }
        }
        a.len().cmp(&b.len())
    }
}

/// `RTRIM`: compares as BINARY but ignores trailing spaces.
pub struct RTrimCollation;

impl Collation for RTrimCollation {
    fn name(&self) -> &str {
        "RTRIM"
    }
    fn compare(&self, a: &str, b: &str) -> Ordering {
        let a = a.trim_end_matches(' ');
        let b = b.trim_end_matches(' ');
        a.as_bytes().cmp(b.as_bytes())
    }
}

/// Compare two [`Value`]s with a collation applied to TEXT comparison.
///
/// Non-text values keep the engine's total order (`NULL < numbers < text
/// < blob`); only text-text pairs route through the collation.
pub fn compare_collated(a: &Value, b: &Value, coll: &dyn Collation) -> Ordering {
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => coll.compare(x.as_str(), y.as_str()),
        _ => a.cmp(b),
    }
}

/// Fold a value through a collation into its BYTE-ORDER INDEX-KEY form.
///
/// The engine's B+trees compare encoded keys bytewise with no comparator
/// hook, so a collated index (e.g. `CREATE INDEX i ON t(v COLLATE NOCASE)`)
/// must bake the collation into the key itself: NOCASE lowercases ASCII,
/// RTRIM strips trailing spaces. Both folds are order-preserving for
/// ASCII text, so range scans stay correct. Custom plugin collations have
/// no byte-preserving fold — they fall back to BINARY keys (documented
/// limitation; comparisons in WHERE still honor them).
///
/// Every index-key encode site (maintenance, point lookup, IN-list,
/// range bounds, INLJ probes) MUST pass values through this so probe keys
/// and stored keys agree.
///
/// Borrowing variant used on hot paths (no clone for BINARY keys).
#[inline]
pub fn collation_fold_key_ref<'a>(collation: &str, v: &'a Value) -> std::borrow::Cow<'a, Value> {
    if collation.eq_ignore_ascii_case("BINARY") || !matches!(v, Value::Text(_)) {
        return std::borrow::Cow::Borrowed(v);
    }
    if collation.eq_ignore_ascii_case("NOCASE") {
        if let Value::Text(t) = v {
            // ASCII-only fold (SQLite's NOCASE is ASCII-only).
            let folded: String = t.as_str().chars().map(|c| c.to_ascii_lowercase()).collect();
            return std::borrow::Cow::Owned(Value::Text(folded.into()));
        }
    } else if collation.eq_ignore_ascii_case("RTRIM") {
        if let Value::Text(t) = v {
            let trimmed = t.as_str().trim_end_matches(' ');
            return std::borrow::Cow::Owned(Value::Text(trimmed.to_string().into()));
        }
    }
    // Unknown / custom collation: BINARY key form.
    std::borrow::Cow::Borrowed(v)
}

#[inline]
pub fn collation_fold_key(collation: &str, v: &Value) -> Value {
    collation_fold_key_ref(collation, v).into_owned()
}

/// Fold a multi-column index key: values are paired with the index
/// columns' collations (missing columns fold as-is / NULL).
pub fn collation_fold_index_key(
    columns: &[crate::schema::IndexColumn],
    values: &[Value],
) -> Vec<Value> {
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            columns
                .get(i)
                .map(|c| collation_fold_key(&c.collation, v))
                .unwrap_or_else(|| v.clone())
        })
        .collect()
}

/// Encode a full multi-column index key with collation folding, straight
/// into a byte buffer (probe keys and maintenance keys share this).
pub fn encode_collated_index_key_into(
    columns: &[crate::schema::IndexColumn],
    values: &[Value],
    out: &mut Vec<u8>,
) {
    for (i, v) in values.iter().enumerate() {
        match columns.get(i) {
            Some(c) => collation_fold_key_ref(&c.collation, v)
                .encode_order_key_into(out),
            None => v.encode_order_key_into(out),
        }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// The per-Database plugin registry. Immutable-after-build: registration
/// methods on `Database` clone the `Arc`, replace one map, and store the
/// new snapshot, so executing statements keep a consistent view.
#[derive(Default, Clone)]
pub struct PluginRegistry {
    /// lowercase name → function
    scalars: Arc<HashMap<String, Arc<dyn ScalarFunction>>>,
    aggregates: Arc<HashMap<String, Arc<dyn AggregateFunction>>>,
    collations: Arc<HashMap<String, Arc<dyn Collation>>>,
    /// virtual-table module names (lowercase)
    modules: Arc<HashMap<String, Arc<dyn VirtualTableModule>>>,
    /// page-codec names (lowercase)
    codecs: Arc<HashMap<String, Arc<dyn PageCodec>>>,
    /// Bumped on every mutation (cache invalidation).
    pub(crate) version: u64,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            scalars: Arc::new(HashMap::new()),
            aggregates: Arc::new(HashMap::new()),
            collations: Arc::new(HashMap::new()),
            modules: Arc::new(HashMap::new()),
            codecs: Arc::new(HashMap::new()),
            version: 0,
        }
    }

    pub fn scalar(&self, name: &str) -> Option<Arc<dyn ScalarFunction>> {
        self.scalars.get(&name.to_ascii_lowercase()).cloned()
    }
    pub fn aggregate(&self, name: &str) -> Option<Arc<dyn AggregateFunction>> {
        self.aggregates.get(&name.to_ascii_lowercase()).cloned()
    }
    pub fn collation(&self, name: &str) -> Option<Arc<dyn Collation>> {
        self.collations.get(&name.to_ascii_lowercase()).cloned()
    }
    pub fn module(&self, name: &str) -> Option<Arc<dyn VirtualTableModule>> {
        self.modules.get(&name.to_ascii_lowercase()).cloned()
    }
    pub fn codec(&self, name: &str) -> Option<Arc<dyn PageCodec>> {
        self.codecs.get(&name.to_ascii_lowercase()).cloned()
    }

    /// Replace-or-insert a scalar function. The map is copied only when the
    /// Arc is shared with a running statement (copy-on-write).
    pub fn set_scalar(&mut self, f: Arc<dyn ScalarFunction>) {
        let m = Arc::make_mut(&mut self.scalars);
        m.insert(f.name().to_ascii_lowercase(), f);
        self.version += 1;
    }
    pub fn set_aggregate(&mut self, f: Arc<dyn AggregateFunction>) {
        let m = Arc::make_mut(&mut self.aggregates);
        m.insert(f.name().to_ascii_lowercase(), f);
        self.version += 1;
    }
    pub fn set_collation(&mut self, c: Arc<dyn Collation>) {
        let m = Arc::make_mut(&mut self.collations);
        m.insert(c.name().to_ascii_lowercase(), c);
        self.version += 1;
    }
    pub fn set_module(&mut self, m: Arc<dyn VirtualTableModule>) {
        let mm = Arc::make_mut(&mut self.modules);
        mm.insert(m.name().to_ascii_lowercase(), m);
        self.version += 1;
    }
    pub fn set_codec(&mut self, c: Arc<dyn PageCodec>) {
        let m = Arc::make_mut(&mut self.codecs);
        m.insert(c.name().to_ascii_lowercase(), c);
        self.version += 1;
    }

    /// Remove a scalar function by name (returns whether it existed).
    pub fn remove_scalar(&mut self, name: &str) -> bool {
        let m = Arc::make_mut(&mut self.scalars);
        let removed = m.remove(&name.to_ascii_lowercase()).is_some();
        if removed {
            self.version += 1;
        }
        removed
    }

    /// Names of all registered functions (scalars + aggregates, sorted).
    pub fn function_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.scalars.keys().cloned().collect();
        v.extend(self.aggregates.keys().cloned());
        v.sort();
        v
    }
}

// ---------------------------------------------------------------------------
// Thread-local scope (mirrors executor::corr::Guard)
// ---------------------------------------------------------------------------

mod scope {
    use super::PluginRegistry;
    use std::cell::RefCell;
    use std::sync::Arc;

    struct ScopeState {
        registry: Option<Arc<PluginRegistry>>,
        depth: u32,
    }

    thread_local! {
        static SCOPE: RefCell<ScopeState> = const { RefCell::new(ScopeState {
            registry: None,
            depth: 0,
        }) };
    }

    /// RAII guard installing a plugin-registry snapshot for the current
    /// statement. Nested installs keep the outermost registry (the
    /// correlated-subquery bridge can re-enter `evaluate` while a plugin
    /// function itself runs a subquery — the outer registry must survive).
    pub struct Guard {
        installed: bool,
    }

    impl Guard {
        pub fn install(reg: Arc<PluginRegistry>) -> Guard {
            let installed = SCOPE.with(|s| {
                let mut s = s.borrow_mut();
                s.depth += 1;
                if s.depth == 1 {
                    s.registry = Some(reg);
                    true
                } else {
                    false
                }
            });
            Guard { installed }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            SCOPE.with(|s| {
                let mut s = s.borrow_mut();
                if self.installed {
                    s.registry = None;
                }
                s.depth = s.depth.saturating_sub(1);
            });
        }
    }

    /// Look up the registry snapshot for the current thread, if a statement
    /// scope is active. One thread-local read + Arc clone.
    pub(crate) fn current() -> Option<Arc<PluginRegistry>> {
        SCOPE.with(|s| s.borrow().registry.clone())
    }
}

pub use scope::Guard as PluginScopeGuard;

/// Resolve a user scalar function for the current statement scope.
pub(crate) fn lookup_scalar(name: &str) -> Option<Arc<dyn ScalarFunction>> {
    scope::current().and_then(|r| r.scalar(name))
}

/// Resolve a user aggregate function for the current statement scope.
pub(crate) fn lookup_aggregate(name: &str) -> Option<Arc<dyn AggregateFunction>> {
    scope::current().and_then(|r| r.aggregate(name))
}

/// Resolve a virtual-table module for the current statement scope.
pub(crate) fn lookup_module(name: &str) -> Option<Arc<dyn VirtualTableModule>> {
    scope::current().and_then(|r| r.module(name))
}

/// Whether a name collides with a built-in scalar (prevents accidental
/// override of engine internals through `create_function`).
pub(crate) fn lookup_scalar_is_builtin(name: &str) -> bool {
    crate::executor::expr::is_builtin_scalar(name)
}

/// Resolve a registered page codec for the current statement scope.
pub(crate) fn lookup_codec(name: &str) -> Option<Arc<dyn PageCodec>> {
    scope::current().and_then(|r| r.codec(name))
}

/// Resolve a collation (built-ins NOCASE/RTRIM are always available;
/// BINARY is the engine's default order and needs no object).
pub(crate) fn lookup_collation(name: &str) -> Option<Arc<dyn Collation>> {
    match name.to_ascii_lowercase().as_str() {
        "binary" => None, // BINARY = Value's default ordering
        "nocase" => Some(Arc::new(NoCaseCollation)),
        "rtrim" => Some(Arc::new(RTrimCollation)),
        other => scope::current().and_then(|r| r.collation(other)),
    }
}

/// Call a user scalar function by name (statement scope must be active).
/// Returns `None` when no function is registered under that name.
pub(crate) fn call_user_scalar(name: &str, args: &[Value]) -> Option<Result<Value>> {
    let f = lookup_scalar(name)?;
    if !f.arity().accepts(args.len()) {
        return Some(Err(Error::semantic(format!(
            "wrong number of arguments to function {}(): {} supplied",
            name,
            args.len()
        ))));
    }
    let ctx = FnCtx::new(args.len());
    Some(f.call(&ctx, args))
}
