#![allow(unexpected_cfgs)]

//! Type information, values, and arguments for the native sqlx driver.
//!
//! The mapping mirrors SQLite's dynamic type system exactly:
//!
//! | Rust type          | SQL type  |
//! |--------------------|-----------|
//! | `bool`             | BOOLEAN   |
//! | `i8`/`i16`/`i32`   | INT4      |
//! | `i64`/`u32`/`u64`  | INTEGER   |
//! | `f32`/`f64`        | REAL      |
//! | `&str`/`String`    | TEXT      |
//! | `&[u8]`/`Vec<u8>`  | BLOB      |

use std::borrow::Cow;
use std::str::FromStr;

use sqlx_core::decode::Decode;
use sqlx_core::encode::{Encode, IsNull};
use sqlx_core::error::BoxDynError;
use sqlx_core::type_info::TypeInfo;
use sqlx_core::types::Type;
use sqlx_core::value::{Value, ValueRef};

use crate::types::Text;
use crate::types::Value as EngineValue;

use crate::sqlx_driver::Rustqlite;

// ---------------------------------------------------------------------------
// Type info
// ---------------------------------------------------------------------------

/// The data type of a value or column, mirroring SQLite's storage classes
/// plus the affinity-derived extensions sqlx's SQLite driver uses.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum DataType {
    /// SQL NULL.
    Null,
    /// INTEGER storage class (SQLite has no integer widths; `i64` is the
    /// only safe mapping).
    Integer,
    /// REAL storage class.
    Float,
    /// TEXT storage class.
    Text,
    /// BLOB storage class.
    Blob,
    /// Declared type `BOOLEAN`.
    Bool,
    /// Declared type `INT4`; hints the macros to use `i32`.
    Int4,
    /// Declared type `DATE`.
    Date,
    /// Declared type `TIME`.
    Time,
    /// Declared type `DATETIME` / `TIMESTAMP`.
    Datetime,
}

/// Type information for a rustqlite type.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct RustqliteTypeInfo(pub(crate) DataType);

impl std::fmt::Display for RustqliteTypeInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.name())
    }
}

impl TypeInfo for RustqliteTypeInfo {
    fn is_null(&self) -> bool {
        matches!(self.0, DataType::Null)
    }

    fn name(&self) -> &str {
        match self.0 {
            DataType::Null => "NULL",
            DataType::Text => "TEXT",
            DataType::Float => "REAL",
            DataType::Blob => "BLOB",
            DataType::Int4 | DataType::Integer => "INTEGER",
            DataType::Bool => "BOOLEAN",
            DataType::Date => "DATE",
            DataType::Time => "TIME",
            DataType::Datetime => "DATETIME",
        }
    }
}

impl DataType {
    /// The runtime type of an engine value (SQLite's `sqlite3_column_type`).
    pub(crate) fn of(value: &EngineValue) -> Self {
        match value {
            EngineValue::Null => DataType::Null,
            EngineValue::Integer(_) => DataType::Integer,
            EngineValue::Real(_) => DataType::Float,
            EngineValue::Text(_) => DataType::Text,
            EngineValue::Blob(_) => DataType::Blob,
        }
    }
}

/// Affinity-based parsing of a *declared* column type, byte-compatible
/// with sqlx's SQLite driver (and SQLite's affinity rules).
// note: this implementation is particularly important as this is how the
//       macros would determine what Rust type maps to what *declared* SQL type
// <https://www.sqlite.org/datatype3.html#affname>
impl FromStr for DataType {
    type Err = BoxDynError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.to_ascii_lowercase();
        Ok(match &*s {
            "int4" => DataType::Int4,
            "int8" => DataType::Integer,
            "boolean" | "bool" => DataType::Bool,

            "date" => DataType::Date,
            "time" => DataType::Time,
            "datetime" | "timestamp" => DataType::Datetime,

            _ if s.contains("int") => DataType::Integer,

            _ if s.contains("char") || s.contains("clob") || s.contains("text") => DataType::Text,

            _ if s.contains("blob") => DataType::Blob,

            _ if s.contains("real") || s.contains("floa") || s.contains("doub") => DataType::Float,

            _ => {
                return Err(format!("unknown type: `{s}`").into());
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// An owned value from the database.
///
/// Unlike the C SQLite driver (where `sqlite3_value` is a stateful C object
/// requiring `unsafe impl Send/Sync` and borrow-tracking), this is a plain
/// owned engine value — fully `Send + Sync` with no unsafe anywhere.
#[derive(Clone, Debug)]
pub struct RustqliteValue {
    pub(crate) value: EngineValue,
}

/// A borrowed reference to a single value.
#[derive(Clone, Debug)]
pub struct RustqliteValueRef<'r> {
    pub(crate) value: Cow<'r, EngineValue>,
}

impl<'r> RustqliteValueRef<'r> {
    pub(crate) fn value(value: &'r RustqliteValue) -> Self {
        Self {
            value: Cow::Borrowed(&value.value),
        }
    }
}

impl Value for RustqliteValue {
    type Database = Rustqlite;

    fn as_ref(&self) -> RustqliteValueRef<'_> {
        RustqliteValueRef::value(self)
    }

    fn type_info(&self) -> Cow<'_, RustqliteTypeInfo> {
        Cow::Owned(RustqliteTypeInfo(DataType::of(&self.value)))
    }

    fn is_null(&self) -> bool {
        matches!(self.value, EngineValue::Null)
    }
}

impl ValueRef<'_> for RustqliteValueRef<'_> {
    type Database = Rustqlite;

    fn to_owned(&self) -> RustqliteValue {
        RustqliteValue {
            value: self.value.clone().into_owned(),
        }
    }

    fn type_info(&self) -> Cow<'_, RustqliteTypeInfo> {
        Cow::Owned(RustqliteTypeInfo(DataType::of(self.value.as_ref())))
    }

    fn is_null(&self) -> bool {
        matches!(self.value.as_ref(), EngineValue::Null)
    }
}

// Decoding accessors (private, mirroring the SQLite driver's shape).
// Text/blob accessors take `self` by value so borrows can carry the `'r`
// lifetime of the underlying row data (Decode consumes the ValueRef).
impl<'r> RustqliteValueRef<'r> {
    pub(super) fn int64(&self) -> Result<i64, BoxDynError> {
        match self.value.as_ref() {
            EngineValue::Integer(i) => Ok(*i),
            // SQLite's sqlite3_value_int64() truncates REALs.
            EngineValue::Real(f) => Ok(*f as i64),
            other => Err(format!(
                "unexpected type {} decoding as i64",
                RustqliteTypeInfo(DataType::of(other))
            )
            .into()),
        }
    }

    pub(super) fn double(&self) -> Result<f64, BoxDynError> {
        match self.value.as_ref() {
            EngineValue::Real(f) => Ok(*f),
            EngineValue::Integer(i) => Ok(*i as f64),
            other => Err(format!(
                "unexpected type {} decoding as f64",
                RustqliteTypeInfo(DataType::of(other))
            )
            .into()),
        }
    }

    pub(super) fn text(self) -> Result<Cow<'r, str>, BoxDynError> {
        match self.value {
            Cow::Borrowed(EngineValue::Text(t)) => Ok(Cow::Borrowed(t.as_str())),
            Cow::Owned(EngineValue::Text(t)) => Ok(Cow::Owned(t.into_string())),
            other => Err(format!(
                "unexpected type {} decoding as String",
                RustqliteTypeInfo(DataType::of(other.as_ref()))
            )
            .into()),
        }
    }

    pub(super) fn blob(self) -> Result<&'r [u8], BoxDynError> {
        match self.value {
            Cow::Borrowed(EngineValue::Blob(b)) => Ok(b),
            Cow::Owned(_) => {
                Err("cannot borrow a blob from an owned value; decode as Vec<u8>".into())
            }
            other => Err(format!(
                "unexpected type {} decoding as Vec<u8>",
                RustqliteTypeInfo(DataType::of(other.as_ref()))
            )
            .into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

/// A tuple of arguments to be sent to the database.
///
/// The argument buffer is a plain `Vec<engine::Value>` — binding is a push,
/// with no FFI marshalling and no per-value allocation beyond the vector
/// growth.
#[derive(Debug, Clone, Default)]
pub struct RustqliteArguments {
    pub(crate) values: Vec<EngineValue>,
}

impl sqlx_core::arguments::Arguments for RustqliteArguments {
    type Database = Rustqlite;

    fn reserve(&mut self, len: usize, _size: usize) {
        self.values.reserve(len);
    }

    fn add<'t, T>(&mut self, value: T) -> Result<(), BoxDynError>
    where
        T: Encode<'t, Self::Database> + Type<Self::Database>,
    {
        let len = self.values.len();
        match value.encode(&mut self.values) {
            Ok(IsNull::Yes) => self.values.push(EngineValue::Null),
            Ok(IsNull::No) => {}
            Err(error) => {
                // reset the buffer so we don't leave a half-encoded value
                self.values.truncate(len);
                return Err(error);
            }
        }
        Ok(())
    }

    fn len(&self) -> usize {
        self.values.len()
    }
}

// ---------------------------------------------------------------------------
// Type / Encode / Decode impls for Rust types
// ---------------------------------------------------------------------------

impl Type<Rustqlite> for i8 {
    fn type_info() -> RustqliteTypeInfo {
        RustqliteTypeInfo(DataType::Int4)
    }

    fn compatible(ty: &RustqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Int4 | DataType::Integer)
    }
}

impl Encode<'_, Rustqlite> for i8 {
    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Integer(i64::from(*self)));
        Ok(IsNull::No)
    }
}

impl Decode<'_, Rustqlite> for i8 {
    fn decode(value: RustqliteValueRef<'_>) -> Result<Self, BoxDynError> {
        Ok(value.int64()?.try_into()?)
    }
}

impl Type<Rustqlite> for i16 {
    fn type_info() -> RustqliteTypeInfo {
        RustqliteTypeInfo(DataType::Int4)
    }

    fn compatible(ty: &RustqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Int4 | DataType::Integer)
    }
}

impl Encode<'_, Rustqlite> for i16 {
    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Integer(i64::from(*self)));
        Ok(IsNull::No)
    }
}

impl Decode<'_, Rustqlite> for i16 {
    fn decode(value: RustqliteValueRef<'_>) -> Result<Self, BoxDynError> {
        Ok(value.int64()?.try_into()?)
    }
}

impl Type<Rustqlite> for i32 {
    fn type_info() -> RustqliteTypeInfo {
        RustqliteTypeInfo(DataType::Int4)
    }

    fn compatible(ty: &RustqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Int4 | DataType::Integer)
    }
}

impl Encode<'_, Rustqlite> for i32 {
    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Integer(i64::from(*self)));
        Ok(IsNull::No)
    }
}

impl Decode<'_, Rustqlite> for i32 {
    fn decode(value: RustqliteValueRef<'_>) -> Result<Self, BoxDynError> {
        Ok(value.int64()?.try_into()?)
    }
}

impl Type<Rustqlite> for i64 {
    fn type_info() -> RustqliteTypeInfo {
        RustqliteTypeInfo(DataType::Integer)
    }

    fn compatible(ty: &RustqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Int4 | DataType::Integer)
    }
}

impl Encode<'_, Rustqlite> for i64 {
    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Integer(*self));
        Ok(IsNull::No)
    }
}

impl Decode<'_, Rustqlite> for i64 {
    fn decode(value: RustqliteValueRef<'_>) -> Result<Self, BoxDynError> {
        value.int64()
    }
}

impl Type<Rustqlite> for u8 {
    fn type_info() -> RustqliteTypeInfo {
        RustqliteTypeInfo(DataType::Int4)
    }

    fn compatible(ty: &RustqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Int4 | DataType::Integer)
    }
}

impl Encode<'_, Rustqlite> for u8 {
    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Integer(i64::from(*self)));
        Ok(IsNull::No)
    }
}

impl Decode<'_, Rustqlite> for u8 {
    fn decode(value: RustqliteValueRef<'_>) -> Result<Self, BoxDynError> {
        Ok(value.int64()?.try_into()?)
    }
}

impl Type<Rustqlite> for u16 {
    fn type_info() -> RustqliteTypeInfo {
        RustqliteTypeInfo(DataType::Int4)
    }

    fn compatible(ty: &RustqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Int4 | DataType::Integer)
    }
}

impl Encode<'_, Rustqlite> for u16 {
    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Integer(i64::from(*self)));
        Ok(IsNull::No)
    }
}

impl Decode<'_, Rustqlite> for u16 {
    fn decode(value: RustqliteValueRef<'_>) -> Result<Self, BoxDynError> {
        Ok(value.int64()?.try_into()?)
    }
}

impl Type<Rustqlite> for u32 {
    fn type_info() -> RustqliteTypeInfo {
        RustqliteTypeInfo(DataType::Integer)
    }

    fn compatible(ty: &RustqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Int4 | DataType::Integer)
    }
}

impl Encode<'_, Rustqlite> for u32 {
    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Integer(i64::from(*self)));
        Ok(IsNull::No)
    }
}

impl Decode<'_, Rustqlite> for u32 {
    fn decode(value: RustqliteValueRef<'_>) -> Result<Self, BoxDynError> {
        Ok(value.int64()?.try_into()?)
    }
}

impl Type<Rustqlite> for u64 {
    fn type_info() -> RustqliteTypeInfo {
        RustqliteTypeInfo(DataType::Integer)
    }

    fn compatible(ty: &RustqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Int4 | DataType::Integer)
    }
}

// NOTE: decode-only, matching sqlx's SQLite driver (SQLite has no u64).
impl Decode<'_, Rustqlite> for u64 {
    fn decode(value: RustqliteValueRef<'_>) -> Result<Self, BoxDynError> {
        Ok(value.int64()?.try_into()?)
    }
}

impl Type<Rustqlite> for f32 {
    fn type_info() -> RustqliteTypeInfo {
        RustqliteTypeInfo(DataType::Float)
    }
}

impl Encode<'_, Rustqlite> for f32 {
    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Real(f64::from(*self)));
        Ok(IsNull::No)
    }
}

impl Decode<'_, Rustqlite> for f32 {
    fn decode(value: RustqliteValueRef<'_>) -> Result<Self, BoxDynError> {
        #[allow(clippy::cast_possible_truncation)]
        Ok(value.double()? as f32)
    }
}

impl Type<Rustqlite> for f64 {
    fn type_info() -> RustqliteTypeInfo {
        RustqliteTypeInfo(DataType::Float)
    }
}

impl Encode<'_, Rustqlite> for f64 {
    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Real(*self));
        Ok(IsNull::No)
    }
}

impl Decode<'_, Rustqlite> for f64 {
    fn decode(value: RustqliteValueRef<'_>) -> Result<Self, BoxDynError> {
        value.double()
    }
}

impl Type<Rustqlite> for bool {
    fn type_info() -> RustqliteTypeInfo {
        RustqliteTypeInfo(DataType::Bool)
    }

    fn compatible(ty: &RustqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Bool | DataType::Int4 | DataType::Integer)
    }
}

impl Encode<'_, Rustqlite> for bool {
    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Integer(i64::from(*self)));
        Ok(IsNull::No)
    }
}

impl Decode<'_, Rustqlite> for bool {
    fn decode(value: RustqliteValueRef<'_>) -> Result<bool, BoxDynError> {
        Ok(value.int64()? != 0)
    }
}

impl Type<Rustqlite> for str {
    fn type_info() -> RustqliteTypeInfo {
        RustqliteTypeInfo(DataType::Text)
    }
}

impl Encode<'_, Rustqlite> for &'_ str {
    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Text(Text::new(self)));
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Rustqlite> for &'r str {
    fn decode(value: RustqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.text()? {
            Cow::Borrowed(s) => Ok(s),
            Cow::Owned(_) => Err("cannot return borrowed string from owned value".into()),
        }
    }
}
impl Type<Rustqlite> for String {
    fn type_info() -> RustqliteTypeInfo {
        <&str as Type<Rustqlite>>::type_info()
    }
}

impl Encode<'_, Rustqlite> for String {
    fn encode(self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Text(Text::new(&self)));
        Ok(IsNull::No)
    }

    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Text(Text::new(self)));
        Ok(IsNull::No)
    }
}

impl Decode<'_, Rustqlite> for String {
    fn decode(value: RustqliteValueRef<'_>) -> Result<Self, BoxDynError> {
        Ok(value.text()?.into_owned())
    }
}

impl Type<Rustqlite> for [u8] {
    fn type_info() -> RustqliteTypeInfo {
        RustqliteTypeInfo(DataType::Blob)
    }

    fn compatible(ty: &RustqliteTypeInfo) -> bool {
        matches!(ty.0, DataType::Blob | DataType::Text)
    }
}
impl Encode<'_, Rustqlite> for &'_ [u8] {
    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Blob(self.to_vec()));
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r, Rustqlite> for &'r [u8] {
    fn decode(value: RustqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        value.blob()
    }
}

impl Type<Rustqlite> for Vec<u8> {
    fn type_info() -> RustqliteTypeInfo {
        <[u8] as Type<Rustqlite>>::type_info()
    }

    fn compatible(ty: &RustqliteTypeInfo) -> bool {
        <[u8] as Type<Rustqlite>>::compatible(ty)
    }
}

impl Encode<'_, Rustqlite> for Vec<u8> {
    fn encode(self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Blob(self));
        Ok(IsNull::No)
    }

    fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Blob(self.clone()));
        Ok(IsNull::No)
    }
}

impl Decode<'_, Rustqlite> for Vec<u8> {
    fn decode(value: RustqliteValueRef<'_>) -> Result<Self, BoxDynError> {
        Ok(value.blob()?.to_vec())
    }
}

// Type-checking support (powers `Debug` formatting of rows and keeps the
// driver in line for future compile-time macro support). The macro expands
// `cfg!(feature = "chrono"/"time"/...)` probes for optional sqlx type
// integrations that this driver does not enable — hence the allow.
sqlx_core::impl_type_checking!(
    Rustqlite {
        bool,
        i32,
        i64,
        f64,
        String,
        Vec<u8>,
    },
    ParamChecking::Weak,
    feature-types: _info => None,
    datetime-types: {
        chrono: {},
        time: {},
    },
    numeric-types: {
        bigdecimal: {},
        rust_decimal: {},
    },
);

// ---------------------------------------------------------------------------
// chrono support (feature `sqlx-chrono`) — sqlx-sqlite-compatible semantics
// ---------------------------------------------------------------------------

#[cfg(feature = "sqlx-chrono")]
mod chrono_support {
    use super::{DataType, EngineValue, Rustqlite, RustqliteTypeInfo, RustqliteValueRef};
    use chrono::{
        DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, NaiveTime, Offset, SecondsFormat,
        TimeZone, Utc,
    };
    use sqlx_core::decode::Decode;
    use sqlx_core::encode::{Encode, IsNull};
    use sqlx_core::error::BoxDynError;
    use sqlx_core::types::Type;

    impl Type<Rustqlite> for NaiveDateTime {
        fn type_info() -> RustqliteTypeInfo {
            RustqliteTypeInfo(DataType::Datetime)
        }

        fn compatible(ty: &RustqliteTypeInfo) -> bool {
            matches!(
                ty.0,
                DataType::Datetime
                    | DataType::Text
                    | DataType::Integer
                    | DataType::Int4
                    | DataType::Float
            )
        }
    }

    impl Type<Rustqlite> for NaiveDate {
        fn type_info() -> RustqliteTypeInfo {
            RustqliteTypeInfo(DataType::Date)
        }

        fn compatible(ty: &RustqliteTypeInfo) -> bool {
            matches!(ty.0, DataType::Date | DataType::Text)
        }
    }

    impl Type<Rustqlite> for NaiveTime {
        fn type_info() -> RustqliteTypeInfo {
            RustqliteTypeInfo(DataType::Time)
        }

        fn compatible(ty: &RustqliteTypeInfo) -> bool {
            matches!(ty.0, DataType::Time | DataType::Text)
        }
    }

    impl<Tz: TimeZone> Type<Rustqlite> for DateTime<Tz> {
        fn type_info() -> RustqliteTypeInfo {
            <NaiveDateTime as Type<Rustqlite>>::type_info()
        }

        fn compatible(ty: &RustqliteTypeInfo) -> bool {
            <NaiveDateTime as Type<Rustqlite>>::compatible(ty)
        }
    }

    fn encode_text(s: String, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
        buf.push(EngineValue::Text(s.into()));
        Ok(IsNull::No)
    }

    impl<Tz: TimeZone> Encode<'_, Rustqlite> for DateTime<Tz>
    where
        Tz::Offset: std::fmt::Display,
    {
        fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
            encode_text(self.to_rfc3339_opts(SecondsFormat::AutoSi, false), buf)
        }
    }

    impl Encode<'_, Rustqlite> for NaiveDateTime {
        fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
            encode_text(self.format("%F %T%.f").to_string(), buf)
        }
    }

    impl Encode<'_, Rustqlite> for NaiveDate {
        fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
            encode_text(self.format("%F").to_string(), buf)
        }
    }

    impl Encode<'_, Rustqlite> for NaiveTime {
        fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
            encode_text(self.format("%T%.f").to_string(), buf)
        }
    }

    impl<'r> Decode<'r, Rustqlite> for DateTime<Utc> {
        fn decode(value: RustqliteValueRef<'r>) -> Result<Self, BoxDynError> {
            Ok(Utc.from_utc_datetime(&decode_datetime(value)?.naive_utc()))
        }
    }

    impl<'r> Decode<'r, Rustqlite> for DateTime<Local> {
        fn decode(value: RustqliteValueRef<'r>) -> Result<Self, BoxDynError> {
            Ok(Local.from_utc_datetime(&decode_datetime(value)?.naive_utc()))
        }
    }

    impl<'r> Decode<'r, Rustqlite> for DateTime<FixedOffset> {
        fn decode(value: RustqliteValueRef<'r>) -> Result<Self, BoxDynError> {
            decode_datetime(value)
        }
    }

    impl<'r> Decode<'r, Rustqlite> for NaiveDateTime {
        fn decode(value: RustqliteValueRef<'r>) -> Result<Self, BoxDynError> {
            decode_datetime(value).map(|dt| dt.naive_local())
        }
    }

    impl<'r> Decode<'r, Rustqlite> for NaiveDate {
        fn decode(value: RustqliteValueRef<'r>) -> Result<Self, BoxDynError> {
            decode_date(value.text()?.as_ref())
        }
    }

    impl<'r> Decode<'r, Rustqlite> for NaiveTime {
        fn decode(value: RustqliteValueRef<'r>) -> Result<Self, BoxDynError> {
            decode_time(value.text()?.as_ref())
        }
    }

    /// Datetime decode — TEXT (RFC3339 then SQLite's documented formats),
    /// INTEGER (unix seconds), or REAL (f64 seconds; both floor and round
    /// strategies tried, like sqlx-sqlite).
    fn decode_datetime(value: RustqliteValueRef<'_>) -> Result<DateTime<FixedOffset>, BoxDynError> {
        let ty = DataType::of(value.value.as_ref());
        let for_err = value.clone();
        let dt = match ty {
            DataType::Text => decode_datetime_from_text(&value.text()?),
            DataType::Int4 | DataType::Integer => decode_datetime_from_int(value.int64()?),
            DataType::Float => decode_datetime_from_float(value.double()?),
            _ => None,
        };
        dt.ok_or_else(move || {
            let t = for_err
                .text()
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| "<non-text>".to_string());
            format!("invalid datetime: {t}").into()
        })
    }

    fn decode_datetime_from_text(value: &str) -> Option<DateTime<FixedOffset>> {
        if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
            return Some(dt);
        }
        // SQLite's documented formats (the same list sqlx-sqlite tries).
        let sqlite_datetime_formats = &[
            "%F %T%.f",
            "%F %R",
            "%F %RZ",
            "%F %R%:z",
            "%F %T%.fZ",
            "%F %T%.f%:z",
            "%FT%R",
            "%FT%RZ",
            "%FT%R%:z",
            "%FT%T%.f",
            "%FT%T%.fZ",
            "%FT%T%.f%:z",
        ];
        for format in sqlite_datetime_formats {
            if let Ok(dt) = DateTime::parse_from_str(value, format) {
                return Some(dt);
            }
            if let Ok(dt) = NaiveDateTime::parse_from_str(value, format) {
                return Some(Utc.fix().from_utc_datetime(&dt));
            }
        }
        None
    }

    fn decode_datetime_from_int(value: i64) -> Option<DateTime<FixedOffset>> {
        DateTime::from_timestamp(value, 0).map(|dt| dt.fixed_offset())
    }

    fn decode_datetime_from_float(value: f64) -> Option<DateTime<FixedOffset>> {
        // 1) floor: seconds + fractional remainder
        let seconds = value.floor();
        let micros = (value.fract() * 1_000_000.0).round() as u32;
        if let Some(dt) = DateTime::from_timestamp(seconds as i64, micros * 1000) {
            return Some(dt.fixed_offset());
        }
        // 2) round to the nearest second
        let seconds = value.round();
        DateTime::from_timestamp(seconds as i64, 0).map(|dt| dt.fixed_offset())
    }

    fn decode_date(value: &str) -> Result<NaiveDate, BoxDynError> {
        NaiveDate::parse_from_str(value, "%F")
            .map_err(|e| format!("error decoding Date from {}; {}", value, e).into())
    }

    fn decode_time(value: &str) -> Result<NaiveTime, BoxDynError> {
        // %T truncates fractional seconds; try the fractional form first.
        NaiveTime::parse_from_str(value, "%T%.f")
            .or_else(|_| NaiveTime::parse_from_str(value, "%T"))
            .map_err(|e| format!("error decoding Time from {}; {}", value, e).into())
    }
}

// ---------------------------------------------------------------------------
// uuid support (feature `sqlx-uuid`) — sqlx-sqlite-compatible semantics
// ---------------------------------------------------------------------------

#[cfg(feature = "sqlx-uuid")]
mod uuid_support {
    use super::{DataType, EngineValue, Rustqlite, RustqliteTypeInfo, RustqliteValueRef};
    use sqlx_core::decode::Decode;
    use sqlx_core::encode::{Encode, IsNull};
    use sqlx_core::error::BoxDynError;
    use sqlx_core::types::Type;
    use uuid::Uuid;

    impl Type<Rustqlite> for Uuid {
        fn type_info() -> RustqliteTypeInfo {
            RustqliteTypeInfo(DataType::Blob)
        }

        fn compatible(ty: &RustqliteTypeInfo) -> bool {
            matches!(ty.0, DataType::Blob | DataType::Text)
        }
    }

    impl Encode<'_, Rustqlite> for Uuid {
        fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
            buf.push(EngineValue::Blob(self.as_bytes().to_vec()));
            Ok(IsNull::No)
        }
    }

    impl Decode<'_, Rustqlite> for Uuid {
        fn decode(value: RustqliteValueRef<'_>) -> Result<Self, BoxDynError> {
            // BLOB(16) is the canonical form; TEXT spellings parse too.
            let ty = DataType::of(value.value.as_ref());
            match ty {
                DataType::Text => {
                    let t = value.text()?;
                    Uuid::parse_str(t.as_ref()).map_err(Into::into)
                }
                _ => Uuid::from_slice(value.blob()?).map_err(Into::into),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Json<T> support (feature `sqlx-json`) — sqlx-sqlite-compatible semantics
// ---------------------------------------------------------------------------

#[cfg(feature = "sqlx-json")]
mod json_support {
    use super::{EngineValue, Rustqlite, RustqliteTypeInfo, RustqliteValueRef};
    use sqlx_core::decode::Decode;
    use sqlx_core::encode::{Encode, IsNull};
    use sqlx_core::error::BoxDynError;
    use sqlx_core::types::{Json, Type};

    impl<T> Type<Rustqlite> for Json<T> {
        fn type_info() -> RustqliteTypeInfo {
            <String as Type<Rustqlite>>::type_info()
        }

        fn compatible(ty: &RustqliteTypeInfo) -> bool {
            <String as Type<Rustqlite>>::compatible(ty)
        }
    }

    impl<T> Encode<'_, Rustqlite> for Json<T>
    where
        T: serde::Serialize,
    {
        fn encode_by_ref(&self, buf: &mut Vec<EngineValue>) -> Result<IsNull, BoxDynError> {
            let text = serde_json::to_string(&self.0)?;
            buf.push(EngineValue::Text(text.into()));
            Ok(IsNull::No)
        }
    }

    impl<'r, T> Decode<'r, Rustqlite> for Json<T>
    where
        T: 'r + serde::Deserialize<'r>,
    {
        fn decode(value: RustqliteValueRef<'r>) -> Result<Self, BoxDynError> {
            // Mirror sqlx-sqlite: decode the column as a BORROWED &str (its
            // lifetime unifies with 'r) and let sqlx-core's hidden
            // `decode_from_string` drive serde_json under that lifetime.
            Self::decode_from_string(Decode::<Rustqlite>::decode(value)?)
        }
    }
}
