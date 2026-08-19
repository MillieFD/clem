/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! Type unfolding logic for schema generation and validation.
//!
//! ---
//!
//! ### Unfolding Arbitrary Types
//!
//! [`msca`](crate) understands **platform-agnostic** primitive types such as `u32` or `f64` out of
//! the box. Platform-dependent types such as `usize` are deliberately omitted to ensure file
//! portability. Arbitrary user-defined algebraic data types such as structs and enums are
//! [unfolded](Unfold) into their primitive [components](Type).
//!
//! - **Terminal nodes** map to contiguous columnar data buffers by name.
//! - **Internal nodes** exist purely for navigation and reconstruction.
//!
//! ### Unsized Types
//!
//! It is not possible to predetermine the disk space required by each instance of an unsized type;
//! there is no guarantee that two [`Vec<T>`] contain the same number of elements. [`msca`](crate)
//! therefore unfolds unsized types into:
//!
//! 1. Columnar `offsets` buffer describing boundaries.
//! 2. Contiguous `data` buffer encoding items.
//!
//! This design ensures **O(1) random access** and avoids per-element pointer chasing. Sequential
//! scans across the contained elements remain linear; leveraging columnar optimisations for SIMD
//! and prefetch.
//!
//! ```text
//! offsets: [3, 6, 6]
//! data:    [a, b, c, d, e, f, g, h]
//! ```
//!
//! The serialized on-disk example above is deserialized into the memory representation below.
//! Implementers must specify which type to use for offset storage based on the number of expected
//! elements.
//!
//! ```text
//! Item 0 → data[..3] → "abc"
//! Item 1 → data[3..6] → "def"
//! Item 2 → data[6..6] → "" (empty)
//! Item 3 → data[6..] → "gh"
//! ```
//!
//! Nested unsized types use **multiple offset layers** alongside a **single data buffer**. This
//! composable design preserves the performance advantages associated with contiguous item storage;
//! namely predictable vectorised traversal. Scanning performance across the contiguous inner
//! `data` buffer is unaffected by deep nesting. The inner offsets buffer is aligned in memory
//! order of traversal to improve cache locality during nested iteration and reduce TLB misses.
//!
//! ```text
//! inner offsets
//! outer offsets
//! data
//! ```
//!
//! Readers can directly query data from a named field – without reconstructing the full type – by
//! reading only the required columnar data buffer. Each schema segment encodes **one** schema, and
//! each `msca` file requires at least **one** schema segment. Multimodality and schema evolution
//! are achieved by appending additional schema segments.

use std::collections::btree_map::{BTreeMap, Entry, OccupiedEntry, VacantEntry};
use std::fmt::{self, Display};
use std::num::NonZeroU64;
use std::{iter, num};

use bitvec::vec::BitVec;
use minicbor::{CborLen, Decode, Encode};
use static_assertions::{assert_eq_size, const_assert_ne};

use self::number::Number;
use crate::accumulate::{self, Accumulate, Descriptor, OptBitVec, OptInSitu, Serialize};
use crate::dataset::Dataset;
use crate::io;
use crate::io::{Buffer, Checksum, Register, Sector};
use crate::manifest::{self, Manifest};
use crate::segment::{Header, Segment, Variant};

/// Shorthand [`OccupiedEntry`] for a [`Schema`][1] that already exists in the [`Schema`].
///
/// [1]: manifest::Schema
type Occupied<'a> = OccupiedEntry<'a, String, manifest::Schema>;

/* ------------------------------------------------------------------------------ Public Exports */

/// A minimal schema **builder** wrapping a [`BTreeMap`] of [`Column`] descriptors keyed by
/// [`name`](String).
///
/// This type does **not** contain the actual schema definition or columnar data buffers; it is a
/// lightweight descriptor for prototype segment initialisation without holding buffer contents in
/// memory. An on-disk [`Schema`][1] segment encodes the schema definition (column names and types)
/// while on-disk [`Data`][2] segments contain the columnar buffers.
///
/// [1]: manifest::Schema
/// [2]: crate::item::Data
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
// NOTE: schema::Schema (public builder) ≠ manifest::Schema (private descriptor).
pub struct Schema {
    /// [`Column`] descriptors keyed by name.
    ///
    /// The [`BTreeMap`] guarantees a stable deterministic column order for consistent binary
    /// encoding and schema comparison.
    #[cbor(n(0), skip_if = "BTreeMap::is_empty")]
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    columns: BTreeMap<String, Column>,
    /// Schema name for retrieval via the [manifest].
    #[cbor(n(1), skip_if = "String::is_empty")]
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "String::is_empty")
    )]
    name: String,
}

impl Schema {
    /// Initialise a new empty [`Schema`] with no columns.
    pub fn new<N>(name: N) -> Self
    where
        String: From<N>,
    {
        Self {
            columns: BTreeMap::new(),
            name: String::from(name),
        }
    }

    /// Add a [`Column`] to [`self`](Schema) with the specified [`name`](String) and [`type`](Type).
    ///
    /// Returns an empty [accumulator](accumulate::Buffer) for **in-memory** data ingestion. This
    /// design ensures schema verification is performed exactly once.
    #[allow(
        private_bounds,
        private_interfaces,
        reason = "restrict I to supported primitive types only"
    )]
    pub fn column<I>(&mut self, name: impl Into<String>) -> Result<accumulate::Buffer<I>, Error>
    where
        I: BitMatch + Clone + Unfold + Send + Sync + 'static,
        Schema: Unfolder<I>,
    {
        let name = name.into();
        let column = Column::new::<I, Schema>();
        match self.columns.entry(name) {
            Entry::Vacant(e) => e.insert(column),
            Entry::Occupied(e) => return Error::Collision { name: e.key().clone() }.into(),
        };
        let acc = accumulate::Buffer::<I>::default();
        Ok(acc)
    }

    /// [`Write`](File::write) [`self`](Schema) to the provided [`Dataset`] and return the on-disk
    /// schema segment [`Sector`].
    ///
    /// ### Deduplication
    ///
    /// Name conflicts are resolved by comparing the new and existing column layouts **before** file
    /// [`IO`](io); returning the existing [`Sector`] if both underlying definitions are identical
    /// or [`Error::Collision`][1] if the underlying definitions differ. New schemas are written
    /// eagerly to disk before being [registered](Register) to the [`Manifest`].
    ///
    /// ### Errors
    ///
    /// - [`Error::Collision`][1] if a different schema is already registered with the same `name`.
    /// - [`io::Error::Io`] if the underlying [write-cycle](io) fails.
    /// - [`io::Error::Number`] if the [size](Serialize::size) overflows `u64`.
    ///
    /// [1]: manifest::Error::Collision
    pub(crate) async fn finish(self, dataset: &mut Dataset) -> Result<Sector, io::Error> {
        let name = self.name.clone();
        match dataset.file.manifest.schemas.entry(name) {
            Entry::Occupied(e) => self.occupied(e).map_err(Into::into),
            Entry::Vacant(..) => dataset.file.write(self).await,
        }
    }

    /// Compare `self` against the provided [`Occupied`] entry for deduplication.
    ///
    /// Returns the existing on-disk schema [`Sector`] if both underlying definitions are identical,
    /// or [`Error::Collision`](manifest::Error::Collision) if the underlying definitions differ.
    fn occupied(&self, entry: Occupied) -> Result<Sector, manifest::Error> {
        match entry.get().columns == self.columns.iter().map(Schema::map).collect() {
            true => Ok(entry.get().sector),
            false => manifest::Error::Collision { name: entry.key().clone() }.into(),
        }
    }

    /// Map the provided [`Key`](K) to a new empty [`manifest::Column`]
    fn map<K, I>(entry: (K, I)) -> (String, manifest::Column)
    where
        K: Into<String>,
        I: Into<Column>,
    {
        let name = entry.0.into();
        let column = entry.1.into().ty.into();
        (name, column)
    }
}

impl Serialize for Schema {
    type Buffer = Vec<u8>;

    fn size(&self) -> Result<NonZeroU64, number::Error> {
        minicbor::len(self).try_into().map(NonZeroU64::new)?.ok_or(number::Error::Zero)
    }

    fn serialize_into<'a>(&self, mut buf: &'a mut [u8]) -> Result<&'a mut [u8], number::Error> {
        // SAFETY: minicbor::encode is infallible when writing to &mut [u8]
        minicbor::encode(self, &mut buf).expect("Infallible schema CBOR encode failed");
        Ok(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, number::Error> {
        let size = self.size()?.get().try_into()?;
        let buf = vec![0u8; size].serialize_push(self)?;
        // NOTE: cannot use static assertion as size is dependent on runtime data accumulation.
        debug_assert_eq!(buf.len(), size, "actual size ≠ predicted size");
        Ok(buf)
    }
}

impl Segment for Schema {
    const VARIANT: Variant = Variant::Schema;

    #[allow(unused_variables, reason = "manifest segment is not aligned")]
    fn wrap(&self, offset: u64) -> Result<Vec<u8>, number::Error> {
        let size = self.size()?.get();
        let full = { size as usize }
            .checked_add(Header::SIZE + size_of::<u64>())
            .ok_or(number::Error::Zero)?;
        let mut buf = vec![u8::MIN; full];
        buf.as_mut_slice()
            .serialize_push(&{ Self::VARIANT as u8 })?
            .serialize_push(&size)?
            .serialize_push(self)?;
        Self::checksum(&mut buf)?;
        Ok(buf)
    }
}

impl Checksum for Schema {}

impl Register for Schema {
    type Error = io::Error;
    type Entry<'m> = VacantEntry<'m, String, manifest::Schema>;

    fn entry<'m>(&self, m: &'m mut Manifest) -> Result<Self::Entry<'m>, io::Error> {
        match m.schemas.entry(self.name.clone()) {
            Entry::Occupied(e) => manifest::Error::Collision { name: e.key().clone() }.into(),
            Entry::Vacant(e) => Ok(e),
        }
    }

    fn register<'a, 'm>(self, s: &'a Sector, e: Self::Entry<'m>) -> Result<&'a Sector, io::Error> {
        let columns = self.columns.iter().map(Schema::map).collect();
        e.insert(manifest::Schema { columns, sector: *s });
        Ok(s)
    }
}

/* ---------------------------------------------------------------------------- Schema Internals */

/// A minimal column **descriptor** that provides type metadata for reading and writing values.
///
/// `Column` does **not** contain the actual buffer data; it is a lightweight descriptor for
/// discovery and random access without holding buffer contents in memory. Data is stored via one
/// or more on-disk data segments, each of which contains a buffer for this column.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
struct Column {
    /// The [`Type`] of values contained within this column.
    #[n(0)]
    ty: Type,
}

impl Column {
    fn new<T, U>() -> Self
    where
        T: Unfold,
        U: Unfolder<T>,
    {
        Self { ty: T::with_unfolder::<U>() }
    }
}

impl From<Type> for Column {
    fn from(ty: Type) -> Self {
        Column { ty }
    }
}

impl From<&Self> for Column {
    fn from(column: &Self) -> Self {
        column.ty.clone().into()
    }
}

/// A minimal type **descriptor** that provides a stable and extensible representation for
/// platform-agnostic Rust primitives; used when walking the type graph for schema encoding.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
#[non_exhaustive] // To accommodate the potential future stabilisation of additional types.
pub enum Type {
    /* ----------------------------------------------------------- Fixed-Size Machine Primitives */
    /// Boolean primitive which can be `true` or `false`.
    #[n(1)]
    Bool,
    /// [Unicode scalar value][1] representing a single character primitive.
    ///
    /// [1]: https://www.unicode.org/glossary/#unicode_scalar_value
    #[n(2)]
    Char,
    /// Rust numeric primitives.
    #[n(3)]
    Number(#[n(0)] Number),
    /* --------------------------------------------------------- Fixed Size Container Primitives */
    /// Optional (nullable) value wrapping one subtype.
    #[n(4)]
    Option {
        /// [`Type`] of the subtype root node.
        #[n(0)]
        subtype: Box<Type>,
    },
    /// Fixed size tuple wrapping an arbitrary number of subtypes.
    // NOTE: reserved tag is not currently registered nor written to disk
    #[n(5)]
    Tuple {
        /// [`Type`] of each subtype root node. [`Vec::len`] returns the arity.
        #[n(0)]
        subtypes: Vec<Type>,
    },
    /* ------------------------------------------------------------ Unsized Container Primitives */
    /// Variable length UTF-8 string encoded as a sequence of bytes.
    #[n(6)]
    String,
    /// Variable length homogenous sequence wrapping one subtype.
    #[n(7)]
    Sequence {
        /// [`Type`] of the subtype root node.
        #[n(0)]
        subtype: Box<Type>,
    },
    /* ------------------------------------------------------------------- Composite Descriptors */
    /// An algebraic data type constructed from [`typed`](Type) fields keyed by [`name`](String).
    ///
    /// The variant describes composite items at runtime but is never written to disk; nested
    /// [`Column`] layouts are flattened during [registration](Schema::column).
    #[n(8)]
    Struct {
        /// Each field [`name`](String) mapped to the field [`Type`]. A [`BTreeMap`] enforces unique
        /// names and deterministic order.
        #[n(0)]
        fields: BTreeMap<String, Type>,
    },
}

impl Type {
    /// A [`Type::Number`] descriptor for the `f32` primitive type.
    pub const F32: Self = Self::Number(Number { kind: number::Kind::Float, size: 4 });

    /// A [`Type::Number`] descriptor for the `f64` primitive type.
    pub const F64: Self = Self::Number(Number { kind: number::Kind::Float, size: 8 });

    /// A [`Type::Number`] descriptor for the `i128` primitive type.
    pub const I128: Self = Self::Number(Number { kind: number::Kind::Int, size: 16 });

    /// A [`Type::Number`] descriptor for the `i16` primitive type.
    pub const I16: Self = Self::Number(Number { kind: number::Kind::Int, size: 2 });

    /// A [`Type::Number`] descriptor for the `i32` primitive type.
    pub const I32: Self = Self::Number(Number { kind: number::Kind::Int, size: 4 });

    /// A [`Type::Number`] descriptor for the `i64` primitive type.
    pub const I64: Self = Self::Number(Number { kind: number::Kind::Int, size: 8 });

    /// A [`Type::Number`] descriptor for the `i8` primitive type.
    pub const I8: Self = Self::Number(Number { kind: number::Kind::Int, size: 1 });

    /// A [`Number`] descriptor for the [`NonZeroI128`](num::NonZeroI128) type.
    pub const NZI128: Self = Self::Number(Number { kind: number::Kind::NonZeroInt, size: 16 });

    /// A [`Number`] descriptor for the [`NonZeroI16`](num::NonZeroI16) type.
    pub const NZI16: Self = Self::Number(Number { kind: number::Kind::NonZeroInt, size: 2 });

    /// A [`Number`] descriptor for the [`NonZeroI32`](num::NonZeroI32) type.
    pub const NZI32: Self = Self::Number(Number { kind: number::Kind::NonZeroInt, size: 4 });

    /// A [`Number`] descriptor for the [`NonZeroI64`](num::NonZeroI64) type.
    pub const NZI64: Self = Self::Number(Number { kind: number::Kind::NonZeroInt, size: 8 });

    /// A [`Number`] descriptor for the [`NonZeroI8`](num::NonZeroI8) type.
    pub const NZI8: Self = Self::Number(Number { kind: number::Kind::NonZeroInt, size: 1 });

    /// A [`Number`] descriptor for the [`NonZeroU128`](num::NonZeroU128) type.
    pub const NZU128: Self = Self::Number(Number { kind: number::Kind::NonZeroUInt, size: 16});

    /// A [`Number`] descriptor for the [`NonZeroU16`](num::NonZeroU16) type.
    pub const NZU16: Self = Self::Number(Number { kind: number::Kind::NonZeroUInt, size: 2 });

    /// A [`Number`] descriptor for the [`NonZeroU32`](num::NonZeroU32) type.
    pub const NZU32: Self = Self::Number(Number { kind: number::Kind::NonZeroUInt, size: 4 });

    /// A [`Number`] descriptor for the [`NonZeroU64`] type.
    pub const NZU64: Self = Self::Number(Number { kind: number::Kind::NonZeroUInt, size: 8 });

    /// A [`Number`] descriptor for the [`NonZeroU8`](num::NonZeroU8) type.
    pub const NZU8: Self = Self::Number(Number { kind: number::Kind::NonZeroUInt, size: 1 });

    /// A [`Type::Number`] descriptor for the `u128` primitive type.
    pub const U128: Self = Self::Number(Number { kind: number::Kind::UInt, size: 16 });

    /// A [`Type::Number`] descriptor for the `u16` primitive type.
    pub const U16: Self = Self::Number(Number { kind: number::Kind::UInt, size: 2 });

    /// A [`Type::Number`] descriptor for the `u32` primitive type.
    pub const U32: Self = Self::Number(Number { kind: number::Kind::UInt, size: 4 });

    /// A [`Type::Number`] descriptor for the `u64` primitive type.
    pub const U64: Self = Self::Number(Number { kind: number::Kind::UInt, size: 8 });

    /// A [`Type::Number`] descriptor for the `u8` primitive type.
    pub const U8: Self = Self::Number(Number { kind: number::Kind::UInt, size: 1 });

    /// Constructor for [`Type::Option`] wrapping the provided subtype.
    pub fn option(subtype: Self) -> Self {
        match subtype {
            // 1. Flatten nested options. Single null bitmap improves on-disk efficiency.
            Self::Option { subtype } => Self::Option { subtype },
            // 2. Box non-option subtypes to prevent unbounded enum size from infinite recursion.
            #[rustfmt::skip] // single line match arm improves readability
            subtype => Self::Option { subtype: Box::new(subtype) },
        }
    }

    /// Constructor for [`Type::Sequence`] wrapping the provided subtype.
    pub fn sequence(subtype: Self) -> Self {
        Self::Sequence { subtype: Box::new(subtype) }
    }
}

impl Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Bool => f.write_str("bool"),
            Self::Char => f.write_str("char"),
            Self::Number(n) => n.fmt(f),
            Self::Option { subtype } => write!(f, "Option<{subtype}>"),
            Self::Tuple { .. } => f.write_str("tuple"),
            Self::String => f.write_str("String"),
            Self::Sequence { subtype } => write!(f, "Vec<{subtype}>"),
            Self::Struct { fields } => write!(f, "Struct {{ {fields:?} }}"),
        }
    }
}

pub mod number {
    //! This module provides a minimal and extensible [`Number`] **descriptor** for Rust numeric
    //! primitives.
    //!
    //! Defining a distinct enum variant for each fixed-width machine primitive type is fragile; as
    //! Rust stabilises new types – such as [`f16`][1] – new enum variants would need to be added,
    //! which may break backwards compatibility and binary encoding.
    //!
    //! Instead, this module defines an extensible [`Number`] descriptor to encode arbitrary numeric
    //! types via a combination of [kind](Kind) and [size](size_of) fields.
    //!
    //! [1]: https://rust-lang.github.io/rfcs/3453-f16-and-f128.html

    use std::convert::Infallible;
    use std::fmt;
    use std::num::TryFromIntError;

    use minicbor::{CborLen, Decode, Encode};

    /// Semantic classification of the numeric primitive type.
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
    #[non_exhaustive] // To accommodate the potential stabilisation of additional numeric kinds.
    pub enum Kind {
        /* ---------------------------------------------------------------------------- Unsigned */
        /// Unsigned integer type.
        #[n(0)]
        UInt,
        /// [Non-zero](core::num::NonZero) unsigned integer type.
        #[n(1)]
        NonZeroUInt,
        /* ------------------------------------------------------------------------------ Signed */
        /// Signed integer type.
        #[n(2)]
        Int,
        /// [Non-zero](core::num::NonZero) signed integer type.
        #[n(3)]
        NonZeroInt,
        /* ---------------------------------------------------------------------- Floating Point */
        /// Floating point type.
        #[n(4)]
        Float,
    }

    impl fmt::Display for Kind {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            match self {
                Self::UInt => f.write_str("u"),
                Self::NonZeroUInt => f.write_str("NonZeroU"),
                Self::Int => f.write_str("i"),
                Self::NonZeroInt => f.write_str("NonZeroI"),
                Self::Float => f.write_str("f"),
            }
        }
    }

    /// A minimal and extensible runtime numeric type **descriptor** that specifies:
    ///
    /// 1. The [kind](Kind) of number.
    /// 2. The [size](size_of) of each value in bytes.
    ///
    /// This type does **not** contain the actual numeric value; it is a lightweight descriptor for
    /// numeric type information without holding values in memory. Each unique combination of `Kind`
    /// and `bytes` corresponds to a specific Rust numeric primitive type.
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
    pub struct Number {
        /// Semantic classification of the numeric primitive type.
        #[n(0)]
        pub kind: Kind,
        /// Number of bytes used to encode each value.
        #[n(1)]
        pub size: u8,
    }

    impl fmt::Display for Number {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "{}{}", self.kind, self.size * 8)
        }
    }

    /* -------------------------------------------------------------------------- Specific Error */

    /// Errors returned by [numerical](self) operations and conversions.
    ///
    /// Enum variants cover various granular error cases that may arise when working with numbers.
    /// Users should consider handling errors explicitly wherever possible to provide meaningful
    /// error messages and recovery actions.
    ///
    /// ### Implementation
    ///
    /// This enum is `#[non_exhaustive]` meaning additional variants may be added in future
    /// versions. Implementers matching on the enum are advised to include a fallback `other` arm to
    /// accommodate additions.
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[derive(Debug, Clone, Eq, PartialEq)]
    #[non_exhaustive] // To accommodate potential future error cases.
    pub enum Error {
        /// Underlying [`TryFromIntError`] from a checked conversion between two types.
        Convert(TryFromIntError),
        /// Attempted to decode a zero value into a [`NonZero`](core::num::NonZero) field.
        Zero,
    }

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            match self {
                Self::Convert(e) => write!(f, "Integer type conversion error → {e}"),
                Self::Zero => write!(f, "Expected non-zero value was zero"),
            }
        }
    }

    impl std::error::Error for Error {}

    impl From<TryFromIntError> for Error {
        fn from(e: TryFromIntError) -> Self {
            Self::Convert(e)
        }
    }

    impl From<Infallible> for Error {
        fn from(e: Infallible) -> Self {
            match e {}
        }
    }

    //noinspection DuplicatedCode → Conversion is implemented for error types in different modules.
    impl<T, E> From<Error> for Result<T, E>
    where
        E: From<Error>,
    {
        fn from(error: Error) -> Self {
            Err(E::from(error))
        }
    }
}

/* ------------------------------------------------------------------------------ Specific Error */

/// Errors returned by [`Schema`] composition.
///
/// Enum variants cover various granular error cases that may arise when working with schemas.
/// Users should consider handling errors explicitly wherever possible to provide meaningful error
/// messages and recovery actions.
///
/// ### Implementation
///
/// This enum is `#[non_exhaustive]` meaning additional variants may be added in future versions.
/// Implementers matching on the enum are advised to include a fallback `other` arm to accommodate
/// additions.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive] // To accommodate potential future error cases.
pub enum Error {
    /// A [`Column`] with the same [name](String) but a different [type](Type) already exists in
    /// the [`Schema`].
    ///
    /// Each schema stores columns in a [`BTreeMap`] keyed by column name. Reusing an existing
    /// name therefore overwrites the existing column definition, resulting in possible data loss.
    Collision {
        /// Name shared by the new and existing columns.
        name: String,
    },
    /// The [`Schema`] does not contain the requested [`Column`] or contains fewer than the
    /// requested number of columns.
    NotFound,
    /// Underlying [`Error`](number::Error) from a numerical operation or conversion.
    Number(number::Error),
    /// The requested type is not supported by this version of [`msca`](crate).
    ///
    /// Some types are deliberately omitted. Please read the [type documentation](Type) for more
    /// details. If you think a type should be supported, please open a new GitHub feature request
    /// with your use-case and justification for inclusion.
    Unsupported(&'static str),
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Collision { name } => write!(f, "Name collision → {name}"),
            Self::NotFound => f.write_str("Column not found in this schema"),
            Self::Number(e) => write!(f, "Number error → {e}"),
            Self::Unsupported(msg) => write!(f, "Unsupported type → {msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<number::Error> for Error {
    fn from(e: number::Error) -> Self {
        Self::Number(e)
    }
}

//noinspection DuplicatedCode → Conversion is implemented for error types in different modules.
impl<T, E> From<Error> for Result<T, E>
where
    E: From<Error>,
{
    fn from(error: Error) -> Self {
        Err(E::from(error))
    }
}

/* --------------------------------------------------------------------- AsType Trait Definition */

/// A **supported type** with a constant on-disk [`Type`] descriptor for compile-time evaluation.
///
/// This trait is exclusively implemented for [msca](crate) supported primitive types and fixes the
/// [`Type`] mapping in one location. A single blanket [`Unfolder`] implementation can serve every
/// supported [primitive][1]. The trait is also usable in a `const` context.
///
/// ### Guidance
///
/// `usize` and `isize` are deliberately omitted: each has a platform-dependent size that is not
/// portable across targets. Refer to the [module documentation](self) for more details.
///
/// This trait is not implemented for wrapper types such as [`Option`] or [`Vec`] due to `const fn`
/// limitations on [`Box::new`].
///
/// [1]: https://doc.rust-lang.org/book/ch03-02-data-types.html
pub(crate) trait AsType {
    /// The on-disk [`Type`] used to describe [`Self`].
    const TYPE: Type;
}

/* ----------------------------------------------------------------- AsType Trait Implementation */

impl AsType for bool {
    const TYPE: Type = Type::Bool;
}

impl AsType for u8 {
    const TYPE: Type = Type::U8;
}

impl AsType for u16 {
    const TYPE: Type = Type::U16;
}

impl AsType for u32 {
    const TYPE: Type = Type::U32;
}

impl AsType for u64 {
    const TYPE: Type = Type::U64;
}

impl AsType for u128 {
    const TYPE: Type = Type::U128;
}

impl AsType for num::NonZeroU8 {
    const TYPE: Type = Type::NZU8;
}

impl AsType for num::NonZeroU16 {
    const TYPE: Type = Type::NZU16;
}

impl AsType for num::NonZeroU32 {
    const TYPE: Type = Type::NZU32;
}

impl AsType for num::NonZeroU64 {
    const TYPE: Type = Type::NZU64;
}

impl AsType for num::NonZeroU128 {
    const TYPE: Type = Type::NZU128;
}

impl AsType for i8 {
    const TYPE: Type = Type::I8;
}

impl AsType for i16 {
    const TYPE: Type = Type::I16;
}

impl AsType for i32 {
    const TYPE: Type = Type::I32;
}

impl AsType for i64 {
    const TYPE: Type = Type::I64;
}

impl AsType for i128 {
    const TYPE: Type = Type::I128;
}

impl AsType for num::NonZeroI8 {
    const TYPE: Type = Type::NZI8;
}

impl AsType for num::NonZeroI16 {
    const TYPE: Type = Type::NZI16;
}

impl AsType for num::NonZeroI32 {
    const TYPE: Type = Type::NZI32;
}

impl AsType for num::NonZeroI64 {
    const TYPE: Type = Type::NZI64;
}

impl AsType for num::NonZeroI128 {
    const TYPE: Type = Type::NZI128;
}

impl AsType for f32 {
    const TYPE: Type = Type::F32;
}

impl AsType for f64 {
    const TYPE: Type = Type::F64;
}

impl AsType for char {
    const TYPE: Type = Type::Char;
}

impl AsType for String {
    const TYPE: Type = Type::String;
}

impl AsType for &str {
    const TYPE: Type = Type::String;
}

/* ------------------------------------------------------------------- BitMatch Trait Definition */

/// Compares two items by their exact **bit pattern**.
///
/// ### BitMatch vs PartialEq
///
/// Each equality-testing trait uses a different underlying mechanism:
///
/// - `BitMatch` compares the serialized on-disk bytes.
/// - [`PartialEq`] compares the logical value.
///
/// For example, two [`f64::NAN`] are bit-identical – meaning [`BitMatch::eq`] returns `true` – but
/// logically non-equivalent – meaning [`PartialEq::eq`] returns `false`.
#[doc(hidden)]
pub trait BitMatch {
    /// Returns `true` if [`self`](Self) and `other` are **bit-identical**.
    fn eq(&self, other: &Self) -> bool;

    /// Returns `true` if [`self`](Self) and `other` are **not** [bit-identical](Self::eq).
    fn ne(&self, other: &Self) -> bool {
        !self.eq(other)
    }
}

/* --------------------------------------------------------------- BitMatch Trait Implementation */

impl BitMatch for bool {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for u8 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for u16 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for u32 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for u64 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for u128 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for num::NonZeroU8 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for num::NonZeroU16 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for num::NonZeroU32 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for num::NonZeroU64 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for num::NonZeroU128 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for i8 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for i16 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for i32 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for i64 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for i128 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for num::NonZeroI8 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for num::NonZeroI16 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for num::NonZeroI32 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for num::NonZeroI64 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for num::NonZeroI128 {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for f32 {
    fn eq(&self, other: &Self) -> bool {
        // NOTE: compare by exact bit pattern unlike PartialEq
        self.to_le_bytes() == other.to_le_bytes()
    }
}

impl BitMatch for f64 {
    fn eq(&self, other: &Self) -> bool {
        // NOTE: compare by exact bit pattern unlike PartialEq
        self.to_le_bytes() == other.to_le_bytes()
    }
}

impl BitMatch for char {
    fn eq(&self, other: &Self) -> bool {
        self == other
    }
}

impl BitMatch for String {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl<I> BitMatch for Option<I>
where
    I: BitMatch,
{
    fn eq(&self, other: &Self) -> bool {
        match self {
            None => other.is_none(),
            Some(a) => other.as_ref().is_some_and(|b| a.eq(b)),
        }
    }
}

impl<I> BitMatch for Vec<I>
where
    I: BitMatch,
{
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().zip(other).all(|both| both.0.eq(both.1))
    }
}
/* --------------------------------------------------------------------- Unfold Trait Definition */

/// A **platform-agnostic primitive type** that is used to record data in a [`Column`].
///
/// [`MSCA`](crate) implements `Unfold` for many Rust primitive and standard library types. The
/// complete list is available [here](crate::schema). Some types are deliberately omitted to
/// preserve cross-platform support.
pub(crate) trait Unfold: Sized {
    /// The [accumulator](Accumulate) type used to ingest values of [`Self`] directly.
    // NOTE: Buffer must be a growable Vec; compiler cannot predict the number of accumulated items
    type RawAcc: Accumulate<Self>
        + Descriptor
        + Serialize<Buffer = Vec<u8>>
        + FromIterator<Self>
        + Default
        + Send
        + Sync
        + 'static;

    /// The [accumulator](Accumulate) type used to ingest [optional](Option) values of [`Self`].
    // NOTE: Buffer must be a growable Vec; compiler cannot predict the number of accumulated items
    type OptAcc: Accumulate<Option<Self>>
        + Descriptor
        + Serialize<Buffer = Vec<u8>>
        + FromIterator<Option<Self>>
        + Default
        + Send
        + Sync
        + 'static;

    /// Delegates to [`unfold`](Unfolder::unfold) on the provided [`Unfolder`].
    fn with_unfolder<U>() -> Type
    where
        U: Unfolder<Self>,
    {
        U::unfold()
    }

    /// Construct an [accumulator](Self::RawAcc) containing exactly **one** [`item`](Self); used to
    /// serialize single-value [`Buffer::Compact`][1] without repeated [`Accumulate::push`] calls.
    ///
    /// [1]: accumulate::Buffer::Compact
    fn once(item: &Self) -> Self::RawAcc
    where
        Self: Clone,
    {
        let i = item.clone();
        iter::once(i).collect()
    }
}

/* ---------------------------------------------------------------- Unfold Trait Size Assertions */

/// Returns the size of `Option<T>` in bytes.
#[rustfmt::skip] // single line function improves readability
pub(crate) const fn size_of_opt<T>() -> usize { size_of::<Option<T>>() }

assert_eq_size!(char, Option<char>);
const_assert_ne!(size_of::<u8>(), size_of_opt::<u8>());
const_assert_ne!(size_of::<u16>(), size_of_opt::<u16>());
const_assert_ne!(size_of::<u32>(), size_of_opt::<u32>());
const_assert_ne!(size_of::<u64>(), size_of_opt::<u64>());
const_assert_ne!(size_of::<u128>(), size_of_opt::<u128>());
assert_eq_size!(num::NonZeroU8, Option<num::NonZeroU8>);
assert_eq_size!(num::NonZeroU16, Option<num::NonZeroU16>);
assert_eq_size!(num::NonZeroU32, Option<num::NonZeroU32>);
assert_eq_size!(num::NonZeroU64, Option<num::NonZeroU64>);
assert_eq_size!(num::NonZeroU128, Option<num::NonZeroU128>);
const_assert_ne!(size_of::<i8>(), size_of_opt::<i8>());
const_assert_ne!(size_of::<i16>(), size_of_opt::<i16>());
const_assert_ne!(size_of::<i32>(), size_of_opt::<i32>());
const_assert_ne!(size_of::<i64>(), size_of_opt::<i64>());
const_assert_ne!(size_of::<i128>(), size_of_opt::<i128>());
assert_eq_size!(num::NonZeroI8, Option<num::NonZeroI8>);
assert_eq_size!(num::NonZeroI16, Option<num::NonZeroI16>);
assert_eq_size!(num::NonZeroI32, Option<num::NonZeroI32>);
assert_eq_size!(num::NonZeroI64, Option<num::NonZeroI64>);
assert_eq_size!(num::NonZeroI128, Option<num::NonZeroI128>);
const_assert_ne!(size_of::<f32>(), size_of_opt::<f32>());
const_assert_ne!(size_of::<f64>(), size_of_opt::<f64>());

/* ----------------------------------------------------------------- Unfold Trait Implementation */

impl Unfold for bool {
    type RawAcc = BitVec;
    type OptAcc = OptBitVec<Self>;
}

impl Unfold for u8 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptBitVec<Self>;
}

impl Unfold for u16 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptBitVec<Self>;
}

impl Unfold for u32 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptBitVec<Self>;
}

impl Unfold for u64 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptBitVec<Self>;
}

impl Unfold for u128 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptBitVec<Self>;
}

impl Unfold for num::NonZeroU8 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptInSitu<Self>;
}

impl Unfold for num::NonZeroU16 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptInSitu<Self>;
}

impl Unfold for num::NonZeroU32 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptInSitu<Self>;
}

impl Unfold for num::NonZeroU64 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptInSitu<Self>;
}

impl Unfold for num::NonZeroU128 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptInSitu<Self>;
}

impl Unfold for i8 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptBitVec<Self>;
}

impl Unfold for i16 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptBitVec<Self>;
}

impl Unfold for i32 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptBitVec<Self>;
}

impl Unfold for i64 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptBitVec<Self>;
}

impl Unfold for i128 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptBitVec<Self>;
}

impl Unfold for num::NonZeroI8 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptInSitu<Self>;
}

impl Unfold for num::NonZeroI16 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptInSitu<Self>;
}

impl Unfold for num::NonZeroI32 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptInSitu<Self>;
}

impl Unfold for num::NonZeroI64 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptInSitu<Self>;
}

impl Unfold for num::NonZeroI128 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptInSitu<Self>;
}

impl Unfold for f32 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptBitVec<Self>;
}

impl Unfold for f64 {
    type RawAcc = Vec<Self>;
    type OptAcc = OptBitVec<Self>;
}

impl Unfold for char {
    type RawAcc = Vec<Self>;
    type OptAcc = OptInSitu<Self>;
}

impl Unfold for String {
    type RawAcc = accumulate::Seq<u8>;
    type OptAcc = accumulate::OptSeq<u8>;
}

impl<I> Unfold for Option<I>
where
    I: Unfold,
{
    type RawAcc = I::OptAcc;
    type OptAcc = accumulate::Flatten<I::OptAcc>;
}

impl<I> Unfold for Vec<I>
where
    I: Clone + Default + Unfold + 'static,
{
    type RawAcc = accumulate::Seq<I>;
    type OptAcc = accumulate::OptSeq<I>;
}

/* ------------------------------------------------------------------- Unfolder Trait Definition */

/// A **schema builder** that can unfold the supported type `I`.
///
/// `Unfolder` is implemented independently for each supported type; enabling type-driven encoding.
/// For example, the default [`Schema`] builder unfolds `u8` into a [`Type::Number`] descriptor.
pub trait Unfolder<I>
where
    I: ?Sized,
{
    /// Returns the [`Type`] descriptor produced by unfolding the supported `I`.
    fn unfold() -> Type;
}

/* --------------------------------------------------------------- Unfolder Trait Implementation */

impl<I> Unfolder<I> for Schema
where
    I: AsType,
{
    fn unfold() -> Type {
        I::TYPE
    }
}

impl<I> Unfolder<Option<I>> for Schema
where
    I: Unfold,
    Schema: Unfolder<I>,
{
    fn unfold() -> Type {
        Type::option(Self::unfold())
    }
}

impl<I> Unfolder<Vec<I>> for Schema
where
    I: Unfold,
    Schema: Unfolder<I>,
{
    fn unfold() -> Type {
        Type::sequence(Self::unfold())
    }
}

/* --------------------------------------------------------------------------------------- Tests */

#[cfg(test)]
mod tests {
    use static_assertions::{assert_impl_all, assert_not_impl_any};

    use super::*;

    /* ---------------------------------------------------------------------------- Shared State */

    /// A [`Schema`] named `test` carrying one `u32` column `a`, shared by the column tests.
    fn schema() -> Schema {
        let mut schema = Schema::new("test");
        schema.column::<u32>("a").expect("failed to initialise test column a");
        schema
    }

    /* ------------------------------------------------------------------------------ Unit Tests */

    /// Reusing a [`Column`] name is **always** rejected as an [`Error::Collision`], whatever the
    /// requested type.
    ///
    /// Two fields registering one column would feed that column from two accumulators, so the
    /// items of one silently overwrite the items of the other. The same-type case is rejected for
    /// exactly that reason; registering one schema twice is deduplicated at segment level instead.
    #[test]
    fn column_reuse_always_collides() {
        let mut schema = schema();
        let same = schema.column::<u32>("a").expect_err("Repeated column accepted");
        let wider = schema.column::<u64>("a").expect_err("Repeated column accepted");
        assert!(matches!(same, Error::Collision { .. }));
        assert!(matches!(wider, Error::Collision { .. }));
    }

    /// [`Type::option`] collapses a nested [`Option`] into a single validity layer.
    #[test]
    fn option_flattens_when_nested() {
        let once = Type::option(Type::U32);
        let twice = Type::option(once.clone());
        assert_eq!(once, twice);
    }

    /// [`Type::sequence`] wraps the provided subtype in a [`Type::Sequence`] descriptor.
    #[test]
    fn sequence_wraps_its_subtype() {
        let expect = Type::Sequence { subtype: Box::new(Type::U8) };
        assert_eq!(Type::sequence(Type::U8), expect);
    }

    /// Every supported type names a corresponding [`Type`], and only those types do. `usize` and `isize`
    /// are deliberately omitted – their width varies by platform – so neither can name a [`Type`],
    /// reach a column, or render a file non-portable.
    #[test]
    fn as_type_excludes_platform_dependent_widths() {
        assert_impl_all!(u8: AsType, BitMatch, Unfold);
        assert_impl_all!(f64: AsType, BitMatch, Unfold);
        assert_impl_all!(bool: AsType, BitMatch, Unfold);
        assert_impl_all!(char: AsType, BitMatch, Unfold);
        assert_impl_all!(String: AsType, BitMatch, Unfold);
        assert_impl_all!(NonZeroU64: AsType, BitMatch, Unfold);
        assert_not_impl_any!(usize: AsType, BitMatch, Unfold);
        assert_not_impl_any!(isize: AsType, BitMatch, Unfold);
    }

    /// Every [`AsType`] constant agrees with the [`Type`] the [`Unfolder`] blanket reports, so the
    /// on-disk descriptor is fixed by the Rust type alone.
    #[test]
    fn as_type_drives_the_unfolder() {
        const TY: Type = f32::TYPE;
        assert_eq!(<Schema as Unfolder<u8>>::unfold(), Type::U8);
        assert_eq!(<Schema as Unfolder<bool>>::unfold(), Type::Bool);
        assert_eq!(<Schema as Unfolder<String>>::unfold(), Type::String);
        assert_eq!(<Schema as Unfolder<f64>>::unfold(), Type::F64);
        assert_eq!(u32::TYPE, Type::U32); // usable in a const context
        assert_eq!(TY, Type::F32);
    }

    /// [`BitMatch::eq`] compares floating point values by exact bit pattern: a repeated
    /// [`NaN`](f64::NAN) payload is bit-identical while distinct payloads are not, unlike
    /// [`PartialEq`].
    #[test]
    fn bit_match_compares_float_bits() {
        let quiet = f32::from_bits(f32::NAN.to_bits() | 1);
        assert!(BitMatch::eq(&f64::NAN, &f64::NAN));
        assert!(BitMatch::eq(&f64::INFINITY, &f64::INFINITY));
        assert!(BitMatch::ne(&f64::INFINITY, &f64::NEG_INFINITY));
        assert!(BitMatch::ne(&1.0f64, &2.0));
        assert!(BitMatch::ne(&f32::NAN, &quiet));
    }

    /// [`BitMatch::eq`] recurses through [`Option`] and [`Vec`] layers so bit-pattern semantics
    /// propagate into optional and unsized columns.
    #[test]
    fn bit_match_recurses_through_layers() {
        assert!(BitMatch::eq(&Some(f64::NAN), &Some(f64::NAN)));
        assert!(BitMatch::eq(&None::<f64>, &None));
        assert!(BitMatch::ne(&Some(f64::NAN), &None));
        assert!(BitMatch::eq(&vec![f32::NAN, 1.0], &vec![f32::NAN, 1.0]));
        assert!(BitMatch::ne(&vec![1.0f32], &vec![1.0, 1.0]));
        assert!(BitMatch::eq(&String::from("a"), &String::from("a")));
    }
}
