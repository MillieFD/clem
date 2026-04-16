/*
Project: clem
GitHub: https://github.com/MillieFD/clem

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! Type graph logic for schema generation and structural matching.
//!
//! ---
//!
//! ### Arbitrary Type Encoding
//!
//! `clem` understands **platform-agnostic** primitive types such as `u32` or `f64`.
//! Platform-dependent types such as `usize` are deliberately omitted to ensure file portability.
//! Additional user-defined types are embedded directly in the schema using a depth-first
//! cursor-based stateful serializer with no per-field allocation on the hot path.
//!
//! - **Leaf nodes** map to contiguous columnar data buffers via index.
//! - **Internal nodes** exist purely for navigation and reconstruction.
//!
//! For example, `struct Outer { foo: Inner, bar: i32 }` and `struct Inner { baz: bool, quux:
//! Option<f64> }` can be encoded as just three contiguous data buffers and one packed null bitmap
//! arranged sequentially:
//!
//! ```text
//! Outer
//! ├─ foo: Inner
//! │  ├─ baz: bool
//! │  │  ╭─ Buffer 0 ─────────╮
//! │  │  │ length: NonZeroU64 │
//! │  │  │ payload: [u8]      │
//! │  │  ╰────────────────────╯
//! │  └─ quux: Option<f64>
//! │     ╭─ Buffer 1 ─────────╮
//! │     │ length: NonZeroU64 │
//! │     │ bitmap: [u8]       │
//! │     │ payload: [f64]     │
//! │     ╰────────────────────╯
//! └─ bar: i32
//!    ╭─ Buffer 2 ─────────╮
//!    │ length: NonZeroU64 │
//!    │ payload: [i32]     │
//!    ╰────────────────────╯
//! ```
//!
//! The schema itself can be conceptualised as a `struct` where each column becomes a field with a
//! `name` and `type`.
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
//!
//! ### Schema Segments
//!
//! To construct a [`Schema`], users must first define a `struct` which implements
//! `serde::Serialize`. Each field becomes a column with a `name` and `type`. Each instance of the
//! struct represents a row. This design moves schema validity checks to compile time by leveraging
//! Rust's type safety, improving data ingestion speed by eliminating costly runtime schema checks.
//! The type tree is serialized into CBOR; encoding the `name` and `type` of all internal and leaf
//! nodes. The user schema struct (root node) defines the schema name which is encoded in the type
//! tree and written to the file manifest `schemas: BTreeMap` to enable schema retrieval by name.
//!
//! ```text
//! Schema Segment
//! ├─ Header
//! │  ├─ variant: u8
//! │  └─ length: NonZeroU64
//! └─ Payload: CBOR
//! ```
//!
//! Readers can directly query data from arbitrary named fields – without reconstructing a type
//! instance – by reading the corresponding columnar data buffer. Each schema segment encodes
//! **one** schema and each `clem` file requires at least **one** schema segment. Multimodality and
//! schema evolution are achieved by appending additional segments.
//!
//! ### 6.3 Schema Validation
//!
//! `Dataset::stream` compares the type graph root node name for `R` against the manifest
//! `schemas: BTreeMap`; initialising a new stream with `schema: R` if no entry exists for the
//! specified name or returning an error if the `R` type graph does not exactly match the existing
//! schema structure.
//!
//! - An exact structural match is required for read and write validity via [`Stream`].
//! - Subset-matches (projections) enable read-only access via [`SubStream`].
//!
//! This design ensures schema verification is performed exactly once. Stream read and write
//! operations can then proceed fearlessly without per-request runtime checks on the hot path.
//! Cloning an existing `Stream<R>` bypasses schema validation. Multi-consumer workloads should
//! therefore prefer cloning a validated stream instead of calling `Dataset::stream` repeatedly.
//! Implementers are encouraged to export canonical types for convenience, removing the need for
//! users to reconstruct schema types manually.

use crate::Error;
use minicbor::{Decode, Encode};
use serde::{Deserialize, Serialize, ser};
use std::any::Any;
use std::collections::BTreeMap;

/* ------------------------------------------------------------------------------ Public Exports */

/// A minimal type **descriptor** that provides a stable and extensible representation for
/// platform-agnostic Rust primitives; used when walking the type graph for schema encoding.
#[derive(Debug, Clone, Eq, Serialize, Deserialize, Encode, Decode)]
#[non_exhaustive] // To accommodate the potential future stabilisation of additional types.
pub(crate) enum Node {
    /* ------------------------------------------------------------ Zero-Size Machine Primitives */
    /// No value.
    ///
    /// `None` is a zero-sized type (ZST) that exists only at the type level and occupies zero bytes
    /// of hardware memory.
    #[n(0)]
    None,
    /// Rust unit `()` primitive; used when no other meaningful type can be returned.
    ///
    /// `Unit` is a zero-sized type (ZST) that exists only at the type level and occupies zero bytes
    /// of hardware memory.
    #[n(1)]
    Unit,
    /// A sentinel type with no values, representing the result of computations that never complete.
    ///
    /// `Never` is a zero-sized type (ZST) that exists only at the type level and occupies zero
    /// bytes of hardware memory.
    #[n(2)]
    Never,
    /* ----------------------------------------------------------- Fixed-Size Machine Primitives */
    /// Rust numeric primitives.
    #[n(3)]
    Number(#[n(0)] number::Number),
    /// Boolean primitive which can be `true` or `false`.
    #[n(4)]
    Bool,
    /* -------------------------------------------------------------------- Container Primitives */
    /// Variable length UTF-8 string encoded as a sequence of bytes.
    #[n(5)]
    String,
    /// Optional (nullable) value wrapping one subgraph.
    #[n(6)]
    Option {
        /// Index of the subgraph root node within the type graph.
        #[n(0)]
        subgraph: u32,
    },
    /// Fixed size tuple wrapping an arbitrary number of subgraphs.
    #[n(7)]
    Tuple {
        /// Index of each subgraph root node within the type graph.
        /// [`Vec::len`] returns the number of elements.
        #[n(0)]
        subgraphs: Vec<u8>,
    },
    /// Variable length sequence wrapping one subgraph.
    #[n(8)]
    Sequence {
        /// Index of the subgraph root node within the type graph.
        #[n(0)]
        subgraph: u32,
    },
    /* -------------------------------------------------------------------- Algebraic Data Types */
    /// Named struct wrapping an arbitrary number of named fields; each described by a subgraph.
    #[n(9)]
    Struct {
        /// Struct name.
        #[n(0)]
        name: String,
        /// Field names mapped to subgraph root node indices within the type graph.
        ///
        /// [`Vec::len`] returns the number of fields. Field order in-memory is not guaranteed to
        /// match CBOR order on-disk.
        #[n(1)]
        fields: BTreeMap<String, u32>,
    },
    /// Named enum wrapping an arbitrary number of variants; each described by a subgraph.
    #[n(10)]
    Enum {
        /// Enum name.
        #[n(0)]
        name: String,
        /// Variant names mapped to subgraph root node indices within the type graph.
        ///
        /// Each variant class maps to a specific subgraph root node:
        ///
        /// - Unit variant with no fields → `None` subgraph.
        /// - Tuple variant with unnamed fields → `Tuple` subgraph.
        /// - Struct variant with named fields → `Struct` subgraph.
        ///
        /// [`Vec::len`] returns the number of fields. Field order in-memory is not guaranteed to
        /// match CBOR order on-disk.
        #[n(1)]
        variants: BTreeMap<String, u32>,
    },
}

impl Node {
    /// Returns `true` if `self` matches the `None` variant.
    fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/* ----------------------------------------------------------------------- Trait Implementations */

impl Default for Node {
    fn default() -> Self {
        Self::None
    }
}

impl PartialEq<Self> for Node {
    fn eq(&self, other: &Self) -> bool {
        self.type_id() == other.type_id()
    }
}

mod number {
    //! Private module provides a minimal stable numeric type **descriptor** for Rust primitives.
    //!
    //! Defining a distinct enum variant for each fixed-width machine primitive type is fragile; as
    //! Rust stabilises new types – such as [`f16`][1] – new enum variants will need to be added,
    //! which may break backwards compatibility and binary encoding.
    //!
    //! Instead, this module defines an extensible [`Number`] descriptor to encode arbitrary numeric
    //! types via a combination of 'kind' and 'size' fields.
    //!
    //! [1]: https://rust-lang.github.io/rfcs/3453-f16-and-f128.html

    use minicbor::{Decode, Encode};
    use serde::{Deserialize, Serialize};
    use std::any::TypeId;

    /// Classification of the numeric primitive type.
    #[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize, Encode, Decode)]
    #[non_exhaustive] // To accommodate the potential stabilisation of additional numeric classes.
    pub(super) enum Kind {
        /* ---------------------------------------------------------------------------- Unsigned */
        /// Unsigned integer type.
        #[n(0)]
        UInt,
        /// Non-zero unsigned integer type.
        #[n(1)]
        NonZeroUInt,
        /* ------------------------------------------------------------------------------ Signed */
        /// Signed integer type.
        #[n(2)]
        Int,
        /// Non-zero signed integer type.
        #[n(3)]
        NonZeroInt,
        /* ---------------------------------------------------------------------- Floating Point */
        /// Floating point type.
        #[n(4)]
        Float,
    }

    /// A minimal and extensible numeric type **descriptor** that specifies:
    ///
    /// 1. The numeric classification.
    /// 2. Number of bytes used to encode the value.
    ///
    /// This type does **not** contain the actual numeric value; it is a lightweight descriptor for
    /// numeric type information without holding values in memory. Each unique combination of `Kind`
    /// and `bytes` corresponds to a specific Rust numeric primitive type.
    #[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize, Encode, Decode)]
    pub(super) struct Number {
        /// Classification of the numeric primitive type.
        #[n(0)]
        kind: Kind,
        /// Number of bytes used to encode the value.
        #[n(1)]
        size: u8,
    }

    impl Number {
        /// Returns the corresponding Rust primitive type for this numeric descriptor.
        #[rustfmt::skip] // Keep match arms single-line for readability.
        pub fn type_id(&self) -> TypeId {
            match self {
                Self { kind: Kind::UInt, size: 1 } => TypeId::of::<u8>(),
                Self { kind: Kind::UInt, size: 2 } => TypeId::of::<u16>(),
                Self { kind: Kind::UInt, size: 4 } => TypeId::of::<u32>(),
                Self { kind: Kind::UInt, size: 8 } => TypeId::of::<u64>(),
                Self { kind: Kind::Int, size: 1 } => TypeId::of::<i8>(),
                Self { kind: Kind::Int, size: 2 } => TypeId::of::<i16>(),
                Self { kind: Kind::Int, size: 4 } => TypeId::of::<i32>(),
                Self { kind: Kind::Int, size: 8 } => TypeId::of::<i64>(),
                Self { kind: Kind::Float, size: 4 } => TypeId::of::<f32>(),
                Self { kind: Kind::Float, size: 8 } => TypeId::of::<f64>(),
                _ => unreachable!("Unrecognised numeric type descriptor → {self:?}"),
            }
        }
    }
}

