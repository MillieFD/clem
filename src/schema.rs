/*
Project: clem
GitHub: https://github.com/MillieFD/clem

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
//! `clem` understands **platform-agnostic** primitive types such as `u32` or `f64` out of the box.
//! Platform-dependent types such as `usize` are deliberately omitted to ensure file portability.
//! Arbitrary user-defined algebraic data types (structs and enums) are [unfolded](Unfold) into
//! their primitive [components](Type).
//!
//! - **Leaf nodes** map to contiguous columnar data buffers via index.
//! - **Internal nodes** exist purely for navigation and reconstruction.
//!
//! ### Unsized Types
//!
//! It is not possible to predetermine the disk space required by each instance of an unsized type;
//! there is no guarantee that one `Vec<T>` contains the same number of elements as another
//! `Vec<T>`. The `clem` type serializer therefore parses unsized types into:
//!
//! 1. Columnar metadata describing boundaries
//! 2. A contiguous region of elements
//!
//! This design ensures **O(1) random access** and avoids per-element pointer chasing. Sequential
//! scans across the contained elements `[T]` remain linear; leveraging columnar optimisations for
//! SIMD and prefetch.
//!
//! ```text
//! offsets: [3, 6, 6]
//! values:  [a, b, c, d, e, f, g, h]
//! ```
//!
//! The serialized on-disk example above is deserialized into the memory representation below.
//! Implementers must specify which type to use for offset storage based on the number of expected
//! elements. A `NonZeroUInt` marker trait is implemented for approved types. The `offsets` buffer
//! can simultaneously encode nullability by leveraging niche-optimisation on non-zero types.
//!
//! ```text
//! Row 0 → values[..3] → "abc"
//! Row 1 → values[3..6] → "def"
//! Row 2 → values[6..6] → "" (empty)
//! Row 3 → values[6..] → "gh"
//! ```
//!
//! Nested unsized types serialize into a flattened graph with **multiple offset layers**. This
//! composable design preserves the performance advantages associated with contiguous value storage;
//! namely predictable vectorised traversal. Scanning performance across the contiguous inner
//! `values` buffer is unaffected by deep nesting. The inner offsets buffer is aligned in memory
//! order of traversal to improve cache locality during nested iteration and reduce TLB misses.
//!
//! ```text
//! inner offsets
//! outer offsets
//! values
//! ```

use minicbor::{Decode, Encode};
use serde::{Deserialize, Serialize};
use std::collections::btree_map::{BTreeMap, Entry, OccupiedEntry, VacantEntry};
use std::convert::Infallible;
use std::fmt::{Display, Formatter};
use std::num;

/// Shorthand occupied entry for a [`Column`] that already exists in the [`Schema`].
type Occupied<'a> = OccupiedEntry<'a, &'static str, Column>;

/// Shorthand vacant entry for a [`Column`] that does not yet exist in the [`Schema`].
type Vacant<'a> = VacantEntry<'a, &'static str, Column>;

/* ------------------------------------------------------------------------------ Public Exports */

/// A minimal schema **builder** wrapping a [`BTreeMap`] of [`Column`] descriptors keyed by name.
///
/// This type does **not** contain the actual schema definition or columnar data buffers; it is a
/// lightweight descriptor for segment initialisation without holding buffer contents in memory. An
/// on-disk schema segment encodes the schema definition (column names and types) while
/// on-disk data segments contain the columnar buffers.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, Encode, Decode)]
// NOTE: schema::Schema (public builder) ≠ manifest::Schema (private descriptor).
pub struct Schema {
    /// [`Column`] descriptors keyed by name.
    ///
    /// The [`BTreeMap`] guarantees a stable deterministic column order for consistent binary
    /// encoding and schema comparison.
    #[cbor(n(1), skip_if = "BTreeMap::is_empty")]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<&'static str, Column>,
}

impl Schema {
    /// Initialises a new empty [`Schema`] with no columns.
    pub fn new() -> Self {
        Self {
            columns: BTreeMap::new(),
        }
    }

    /// Add a [`Column`] to [`self`](Schema) with the specified `name` and [`Type`].
    pub fn column<T: Unfold>(mut self, name: &'static str) -> Result<Self, Error>
    where
        Schema: Unfolder<T, Ok = Type>,
    {
        let ty = T::with_unfolder(&mut self)?;
        match self.columns.entry(name) {
            Entry::Vacant(vacant) => Ok(self.vacant(vacant, ty)),
            Entry::Occupied(occupied) => self.occupied(occupied, ty),
        }
    }

    /// Insert a new [`Column`] into the [`Schema`] at the provided vacant entry.
    fn vacant(mut self, vacant: Vacant, ty: Type) -> Self {
        vacant.insert(Column { ty });
        self // Return self to builder pattern
    }

    /// Resolve a [`Column`] name collision by comparing the associated metadata.
    ///
    /// - Returns `Ok(Self)` unaltered if the column definitions are identical.
    /// - Returns `Err(Error::Collision)` if the column definitions differ.
    fn occupied(mut self, occupied: Occupied, ty: Type) -> Result<Self, Error> {
        match occupied.get().ty == ty {
            // Idempotent column definition
            true => Ok(self),
            // Name collision with type mismatch
            false => Error::collision(occupied, ty).into(),
        }
    }
}

/* ---------------------------------------------------------------------------- Schema Internals */

/// A minimal column **descriptor** that wraps interpretation metadata necessary for random access
/// and value [deserialization](Deserialize).
///
/// This type does **not** contain the actual buffer data; it is a lightweight descriptor for column
/// discovery and access without holding buffer contents in memory. Data is stored via one or more
/// on-disk data segments, each of which contains a buffer for this column.
///
/// [`Vec`] order in-memory is **not** guaranteed to reflect [`Sector`] order on-disk.
#[derive(
    Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize, Encode, Decode,
)]
struct Column {
    /// The [`Type`] of values contained within this column.
    #[n(0)]
    ty: Type,
}

/// A minimal type **descriptor** that provides a stable and extensible representation for
/// platform-agnostic Rust primitives; used when walking the type graph for schema encoding.
#[derive(
    Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, Encode, Decode, Hash,
)]
#[non_exhaustive] // To accommodate the potential future stabilisation of additional types.
pub enum Type {
    /* ----------------------------------------------------------- Fixed-Size Machine Primitives */
    /// Boolean primitive which can be `true` or `false`.
    #[n(0)]
    Bool,
    /// [Unicode scalar value][1] representing a single character primitive.
    ///
    /// [1]: https://www.unicode.org/glossary/#unicode_scalar_value
    #[n(1)]
    Char,
    /// Rust numeric primitives.
    #[n(2)]
    Number(#[n(0)] number::Number),
    /* --------------------------------------------------------- Fixed Size Container Primitives */
    /// Optional (nullable) value wrapping one subtype.
    #[n(3)]
    Option {
        /// [`Type`] of the subtype root node.
        #[n(0)]
        subtype: Box<Type>,
    },
    /// Fixed size tuple wrapping an arbitrary number of subtypes.
    #[n(4)]
    Tuple {
        /// [`Type`] of each subtype root node. [`Vec::len`] returns the number of elements.
        #[n(0)]
        subtypes: Vec<Type>,
    },
    /* ------------------------------------------------------------ Unsized Container Primitives */
    /// Variable length UTF-8 string encoded as a sequence of bytes.
    #[n(5)]
    String,
    /// Variable length homogenous sequence wrapping one subtype.
    #[n(6)]
    Sequence {
        /// [`Type`] of the subtype root node.
        #[n(0)]
        subtype: Box<Type>,
    },
}

impl Type {
    /// A [`Type::Number`] descriptor for the `u8` primitive type.
    pub const U8: Self = Self::Number(number::Number {
        kind: number::Kind::UInt,
        size: 1,
    });

    /// A [`Type::Number`] descriptor for the `u16` primitive type.
    pub const U16: Self = Self::Number(number::Number {
        kind: number::Kind::UInt,
        size: 2,
    });

    /// A [`Type::Number`] descriptor for the `u32` primitive type.
    pub const U32: Self = Self::Number(number::Number {
        kind: number::Kind::UInt,
        size: 4,
    });

    /// A [`Type::Number`] descriptor for the `u64` primitive type.
    pub const U64: Self = Self::Number(number::Number {
        kind: number::Kind::UInt,
        size: 8,
    });

    /// A [`Type::Number`] descriptor for the `u128` primitive type.
    pub const U128: Self = Self::Number(number::Number {
        kind: number::Kind::UInt,
        size: 16,
    });

    /// A [`Number`](number::Number) descriptor for the [`NonZeroU8`](num::NonZeroU8) type.
    pub const NZU8: Self = Self::Number(number::Number {
        kind: number::Kind::NonZeroUInt,
        size: 1,
    });

    /// A [`Number`](number::Number) descriptor for the [`NonZeroU16`](num::NonZeroU16) type.
    pub const NZU16: Self = Self::Number(number::Number {
        kind: number::Kind::NonZeroUInt,
        size: 2,
    });

    /// A [`Number`](number::Number) descriptor for the [`NonZeroU32`](num::NonZeroU32) type.
    pub const NZU32: Self = Self::Number(number::Number {
        kind: number::Kind::NonZeroUInt,
        size: 4,
    });

    /// A [`Number`](number::Number) descriptor for the [`NonZeroU64`](num::NonZeroU64) type.
    pub const NZU64: Self = Self::Number(number::Number {
        kind: number::Kind::NonZeroUInt,
        size: 8,
    });

    /// A [`Number`](number::Number) descriptor for the [`NonZeroU128`](num::NonZeroU128) type.
    pub const NZU128: Self = Self::Number(number::Number {
        kind: number::Kind::NonZeroUInt,
        size: 16,
    });

    /// A [`Type::Number`] descriptor for the `i8` primitive type.
    pub const I8: Self = Self::Number(number::Number {
        kind: number::Kind::Int,
        size: 1,
    });

    /// A [`Type::Number`] descriptor for the `i16` primitive type.
    pub const I16: Self = Self::Number(number::Number {
        kind: number::Kind::Int,
        size: 2,
    });

    /// A [`Type::Number`] descriptor for the `i32` primitive type.
    pub const I32: Self = Self::Number(number::Number {
        kind: number::Kind::Int,
        size: 4,
    });

    /// A [`Type::Number`] descriptor for the `i64` primitive type.
    pub const I64: Self = Self::Number(number::Number {
        kind: number::Kind::Int,
        size: 8,
    });

    /// A [`Type::Number`] descriptor for the `i128` primitive type.
    pub const I128: Self = Self::Number(number::Number {
        kind: number::Kind::Int,
        size: 16,
    });

    /// A [`Number`](number::Number) descriptor for the [`NonZeroI8`](num::NonZeroI8) type.
    pub const NZI8: Self = Self::Number(number::Number {
        kind: number::Kind::NonZeroInt,
        size: 1,
    });

    /// A [`Number`](number::Number) descriptor for the [`NonZeroI16`](num::NonZeroI16) type.
    pub const NZI16: Self = Self::Number(number::Number {
        kind: number::Kind::NonZeroInt,
        size: 2,
    });

    /// A [`Number`](number::Number) descriptor for the [`NonZeroI32`](num::NonZeroI32) type.
    pub const NZI32: Self = Self::Number(number::Number {
        kind: number::Kind::NonZeroInt,
        size: 4,
    });

    /// A [`Number`](number::Number) descriptor for the [`NonZeroI64`](num::NonZeroI64) type.
    pub const NZI64: Self = Self::Number(number::Number {
        kind: number::Kind::NonZeroInt,
        size: 8,
    });

    /// A [`Number`](number::Number) descriptor for the [`NonZeroI128`](num::NonZeroI128) type.
    pub const NZI128: Self = Self::Number(number::Number {
        kind: number::Kind::NonZeroInt,
        size: 16,
    });

    /// A [`Type::Number`] descriptor for the `f32` primitive type.
    pub const F32: Self = Self::Number(number::Number {
        kind: number::Kind::Float,
        size: 4,
    });

    /// A [`Type::Number`] descriptor for the `f64` primitive type.
    pub const F64: Self = Self::Number(number::Number {
        kind: number::Kind::Float,
        size: 8,
    });

    /// Constructor for [`Type::Option`] wrapping the provided subtype.
    pub fn option(subtype: Self) -> Self {
        Self::Option {
            subtype: Box::new(subtype),
        }
    }
}

mod number {
    //! This module provides a minimal stable type **descriptor** for Rust numeric primitives.
    //!
    //! Defining a distinct enum variant for each fixed-width machine primitive type is fragile; as
    //! Rust stabilises new types – such as [`f16`][1] – new enum variants will need to be added,
    //! which may break backwards compatibility and binary encoding.
    //!
    //! Instead, this module defines an extensible [`Number`] descriptor to encode arbitrary numeric
    //! types via a combination of [kind](Kind) and [size](size_of) fields.
    //!
    //! [1]: https://rust-lang.github.io/rfcs/3453-f16-and-f128.html

    use minicbor::{Decode, Encode};
    use serde::{Deserialize, Serialize};
    use std::fmt::{Display, Formatter};

    /// Classification of the numeric primitive type.
    #[derive(
        Debug,
        Copy,
        Clone,
        Eq,
        PartialEq,
        Ord,
        PartialOrd,
        Hash,
        Serialize,
        Deserialize,
        Encode,
        Decode,
    )]
    #[non_exhaustive] // To accommodate the potential stabilisation of additional numeric classes.
    pub(super) enum Kind {
        /* ---------------------------------------------------------------------------- Unsigned */
        /// Unsigned integer type.
        #[n(0)]
        UInt,
        /// [Non-zero](std::num::NonZero) unsigned integer type.
        #[n(1)]
        NonZeroUInt,
        /* ------------------------------------------------------------------------------ Signed */
        /// Signed integer type.
        #[n(2)]
        Int,
        /// [Non-zero](std::num::NonZero) signed integer type.
        #[n(3)]
        NonZeroInt,
        /* ---------------------------------------------------------------------- Floating Point */
        /// Floating point type.
        #[n(4)]
        Float,
    }

    impl Display for Kind {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::UInt => f.write_str("u"),
                Self::NonZeroUInt => f.write_str("NonZeroU"),
                Self::Int => f.write_str("i"),
                Self::NonZeroInt => f.write_str("NonZeroI"),
                Self::Float => f.write_str("f"),
            }
        }
    }

    /// A minimal and extensible numeric type **descriptor** that specifies:
    ///
    /// 1. The numeric [classification](Kind).
    /// 2. Number of bytes used to encode the value.
    ///
    /// This type does **not** contain the actual numeric value; it is a lightweight descriptor for
    /// numeric type information without holding values in memory. Each unique combination of `Kind`
    /// and `bytes` corresponds to a specific Rust numeric primitive type.
    #[derive(
        Debug,
        Copy,
        Clone,
        Eq,
        PartialEq,
        Ord,
        PartialOrd,
        Hash,
        Serialize,
        Deserialize,
        Encode,
        Decode,
    )]
    pub(super) struct Number {
        /// Classification of the numeric primitive type.
        #[n(0)]
        pub kind: Kind,
        /// Number of bytes used to encode the value.
        #[n(1)]
        pub size: u8,
    }

    impl Display for Number {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}{}", self.kind, self.size * 8)
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
/// Implementers are advised to include a wildcard arm `_` to account for potential additions.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[non_exhaustive] // To accommodate potential future error cases.
pub enum Error {
    /// A [`Column`] with the same [name](String) but a different [type](Type) already exists in
    /// the [`Schema`].
    ///
    /// Each schema stores columns in a [`BTreeMap`] keyed by column name. Reusing an existing
    /// name therefore overwrites the existing column definition, resulting in possible data loss.
    Collision {
        /// Name shared by the new and existing columns.
        name: &'static str,
        /// [`Type`] of the existing [`Column`] in the [`Schema`].
        ty1: Type,
        /// [`Type`] of the new [`Column`] being added to the [`Schema`].
        ty2: Type,
    },
    /// The requested type is not supported by this version of [`clem`](crate).
    ///
    /// Some types are deliberately omitted. Please read the [type documentation](Type) for more
    /// details. If you think a type should be supported, please open a new GitHub feature request
    /// with your use case and justification for inclusion.
    Unsupported(&'static str),
}

impl Error {
    /// Returns a new [`Error::Collision`] variant wrapping the column name and conflicting types.
    fn collision(occupied: Occupied, new: Type) -> Self {
        Self::Collision {
            name: occupied.key().clone(),
            ty1: occupied.get().ty,
            ty2: new,
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Collision { name, ty1, ty2 } => write!(
                f,
                "Column collision while building schema:\n\t\
                Tried to add column {{ name: {name}, type {ty1:?} }}\n\t\
                Found existing column {{ name: {name}, type {ty2:?} }}
                "
            ),
            Self::Unsupported(msg) => write!(f, "Unsupported type → {msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl<T, E> From<Error> for Result<T, E>
where
    E: From<Error>,
{
    fn from(error: Error) -> Self {
        Err(E::from(error))
    }
}

/* --------------------------------------------------------------------------- Composition Trait */

/// A platform-agnostic **type** that can be unfolded into its primitive [components](Type) using
/// an [`Unfolder`].
///
/// [`Clem`](crate) provides `Unfold` implementations for many Rust primitive and standard library
/// types. The complete list is [here](crate::schema). All of these types can be unfolded using clem
/// out of the box. Some types are deliberately omitted to preserve cross-platform support.
///
/// Clem provides the [`#[derive(unfold)]`][1] procedural macro to automatically generate `Unfold`
/// implementations for structs and enums in your program. See the [user guide][2] for more details.
///
/// Third-party crates are encouraged to implement `Unfold` on their public types to enable seamless
/// integration with on-disk storage.
// TODO [1] link to procedural macro documentation
// TODO [2] link to procedural macro user guide
pub trait Unfold: Serialize {
    /// Delegates to [`Unfolder::unfold`] on the provided [`Unfolder`].
    fn with_unfolder<Cmp: Unfolder<Self>>(unfolder: &mut Cmp) -> Result<Cmp::Ok, Cmp::Error> {
        unfolder.unfold()
    }
}

/// A **schema builder** that can unfold the supported type `T`.
///
/// - Rust primitive types are unfolded directly into their corresponding [`Type`] descriptors.
/// - Algebraic data types (structs and enums) are unfolded recursively into their component types.
///
/// `Unfolder` is implemented independently for each supported type; enabling type-driven encoding.
/// For example, the default [`Schema`] builder folds `u8` into a [`Type::Number`] descriptor with
/// `kind: number::Kind::UInt` and `size: 1`.
pub trait Unfolder<T: ?Sized> {
    /// The output type returned by [`unfold`] on success.
    type Ok;
    /// The error type returned by [`unfold`] on failure.
    type Error;
    /// Specific unfolding logic for the supported type `T`.
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error>;
}

/* ----------------------------------------------------------------------- Trait Implementations */

impl Unfolder<bool> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::Bool)
    }
}

impl Unfolder<char> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::Char)
    }
}

impl Unfolder<u8> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::U8)
    }
}

impl Unfolder<u16> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::U16)
    }
}

impl Unfolder<u32> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::U32)
    }
}

impl Unfolder<u64> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::U64)
    }
}

impl Unfolder<u128> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::U128)
    }
}

impl Unfolder<num::NonZeroU8> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::NZU8)
    }
}

impl Unfolder<num::NonZeroU16> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::NZU16)
    }
}

impl Unfolder<num::NonZeroU32> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::NZU32)
    }
}

impl Unfolder<num::NonZeroU64> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::NZU64)
    }
}

impl Unfolder<num::NonZeroU128> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::NZU128)
    }
}

impl Unfolder<i8> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::I8)
    }
}

impl Unfolder<i16> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::I16)
    }
}

impl Unfolder<i32> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::I32)
    }
}

impl Unfolder<i64> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::I64)
    }
}

impl Unfolder<i128> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::I128)
    }
}

impl Unfolder<num::NonZeroI8> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::NZI8)
    }
}

impl Unfolder<num::NonZeroI16> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::NZI16)
    }
}

impl Unfolder<num::NonZeroI32> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::NZI32)
    }
}

impl Unfolder<num::NonZeroI64> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::NZI64)
    }
}

impl Unfolder<num::NonZeroI128> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::NZI128)
    }
}

impl Unfolder<f32> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::F32)
    }
}

impl Unfolder<f64> for Schema {
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::F64)
    }
}

impl<T: Unfold> Unfolder<Option<T>> for Schema
where
    Schema: Unfolder<T, Ok = Type>,
{
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::Option {
            subtype: T::with_unfolder(self)?.into(),
        })
    }
}

impl<T: Unfold> Unfolder<Vec<T>> for Schema
where
    Schema: Unfolder<T, Ok = Type>,
{
    type Ok = Type;
    type Error = Infallible;
    fn unfold(&mut self) -> Result<Self::Ok, Self::Error> {
        Ok(Type::Sequence {
            subtype: T::with_unfolder(self)?.into(),
        })
    }
}

impl Unfold for bool {}
impl Unfold for char {}
impl Unfold for u8 {}
impl Unfold for u16 {}
impl Unfold for u32 {}
impl Unfold for u64 {}
impl Unfold for u128 {}
impl Unfold for num::NonZeroU8 {}
impl Unfold for num::NonZeroU16 {}
impl Unfold for num::NonZeroU32 {}
impl Unfold for num::NonZeroU64 {}
impl Unfold for num::NonZeroU128 {}
impl Unfold for i8 {}
impl Unfold for i16 {}
impl Unfold for i32 {}
impl Unfold for i64 {}
impl Unfold for i128 {}
impl Unfold for num::NonZeroI8 {}
impl Unfold for num::NonZeroI16 {}
impl Unfold for num::NonZeroI32 {}
impl Unfold for num::NonZeroI64 {}
impl Unfold for num::NonZeroI128 {}
impl Unfold for f32 {}
impl Unfold for f64 {}
impl<T: Unfold> Unfold for Option<T> {}
impl<T: Unfold> Unfold for Vec<T> {}
