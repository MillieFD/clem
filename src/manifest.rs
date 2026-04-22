/*
Project: clem
GitHub: https://github.com/MillieFD/clem

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! A [`manifest`] footer lists file [`segments`] by type. Data segments are grouped by schema
//! alongside segment-level statistics e.g. min and max values. The manifest acts like the index
//! of a book to enhance:
//!
//! - Segment discovery
//! - Random access
//! - Predicate pruning
//!
//! The manifest is encoded as **CBOR** with definite-length text maps to enable schema and column
//! access by name. A `metadata` key is included when user-specified file-level metadata is present.
//! The manifest is moved and updated when new segments are added.
//!
//! ```text
//! Manifest
//! ├─ metadata (optional)
//! ├─ dictionaries: BTreeMap (optional)
//! └─ schemas: BTreeMap
//! ├─ <schema-name>
//! │  ├─ sector: Sector
//! │  └─ columns: BTreeMap
//! │     ├─ <column-name>
//! │     │  └─ buffers: [Buffer]
//! │     │     ├─ sector: Sector
//! │     │     ├─ count: NonZeroU32
//! │     │     ├─ min: T where T: Ord
//! │     │     └─ max: T where T: Ord
//! │     ⋮
//! │     └─ <final-column>
//! ⋮
//! └─ <final-schema>
//! ```
//!
//! Schema lookup by name returns the corresponding schema segment and a map of all schema columns.
//! A `BTreeMap<String, Schema>` sorted in lexicographic order is used to ensure a fully
//! deterministic layout regardless of insertion order.
//!
//! ```text
//! manifest["schema_name"] → Schema { segment: Segment, columns: BTreeMap<String, Column> }
//! ```
//!
//! Column lookup by name returns the corresponding collection of buffers across all on-disk data
//! segments.
//!
//! ```text
//! manifest["schema_name"]["column_name"] → [Buffer]
//! ```
//!
//! Each `Buffer` contains a `sector: Sector` alongside data statistics such as `min` and `max` for
//! predicate pruning.

use crate::{Error, Sector};
use minicbor::{Decode, Encode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::num::NonZeroUsize;

/* ------------------------------------------------------------------------------ Public Exports */

/// Manifest of file segments and accompanying metadata for random access and predicate pruning.
/// See the [module-level documentation](self) for details.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Encode, Decode)]
#[cbor(tag(100))]
pub(crate) struct Manifest {
    /// Schema segments keyed by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[n(0)]
    pub schemas: BTreeMap<String, Schema>,
    /// Dictionaries keyed by name. Entries are **not** duplicated in the generic `schemas` map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[cbor(n(1), skip_if = "BTreeMap::is_empty")]
    pub dictionaries: BTreeMap<String, Dictionary>,
    /// Indexes keyed by name. Entries are **not** duplicated in the generic `dictionaries` map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[cbor(n(2), skip_if = "BTreeMap::is_empty")]
    pub indexes: BTreeMap<String, Index>,
    /// Implementers can use the optional free-form `metadata.toml` to attach file-level
    /// domain-specific information such as:
    ///
    /// - Date and time
    /// - Experimental parameters
    /// - Provenance
    ///
    /// If a metadata section is included in the file, a corresponding `length` and `offset` are
    /// described in the `manifest`. The core library includes a read and write surface, but
    /// implementers must include their own metadata parsing and validation logic.
    #[cfg(feature = "metadata")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[n(2)]
    pub metadata: Option<Sector>,
}

impl Manifest {
    /// Encode the [`Manifest`] into CBOR bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        // SAFETY: minicbor::encode returns Infallible.
        minicbor::encode(self, &mut buf).expect("Failed to encode manifest as CBOR");
        buf
    }

    /// Decode a [`Manifest`] from CBOR bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        minicbor::decode(bytes).map_err(Error::Decode)
    }
}

/// A minimal schema segment **descriptor** that specifies:
///
/// 1. [`Sector`] where the schema segment is located on disk.
/// 2. [`BTreeMap`] of [`Column`] descriptors keyed by name.
///
/// This type does **not** contain the actual schema definition or columnar data buffers; it is a
/// lightweight descriptor for segment discovery and access without holding buffer contents in
/// memory. An on-disk schema segment encodes the schema definition (column names and types) while
/// on-disk data segments contain the columnar buffers.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub(crate) struct Schema {
    /// Location of the schema segment including header.
    #[n(0)]
    pub sector: Sector,
    /// [`Column`] descriptors keyed by [`name`](String).
    #[cbor(n(1), skip_if = "BTreeMap::is_empty")]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<String, Column>,
}

/// A minimal column **descriptor** that wraps a list of [`Buffer`] descriptors.
///
/// This type does **not** contain the actual buffer data; it is a lightweight descriptor for column
/// discovery and access without holding buffer contents in memory. Data is stored via one or more
/// on-disk data segments, each of which contains a buffer for this column.
///
/// [`Vec`] order in-memory is **not** guaranteed to reflect [`Sector`] order on-disk.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub(crate) struct Column {
    /// List of [`Buffer`] descriptors for this column across all data segments.
    #[cbor(n(0), skip_if = "Vec::is_empty")]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buffers: Vec<Buffer>,
}

/// A minimal columnar buffer **descriptor** that specifies:
///
/// 1. [`Sector`] where the buffer is located on disk.
/// 2. Number of data entries e.g. for index arithmetic.
/// 3. Buffer statistics such as `min` and `max` for predicate pruning.
///
/// This type does **not** contain the actual buffer data; it is a lightweight descriptor for buffer
/// discovery and access without holding buffer contents in memory. Data is stored via contiguous
/// buffers distributed across one or more on-disk data segments.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub(crate) struct Buffer {
    /// Location of the schema segment including header.
    #[n(0)]
    pub sector: Sector,
    /// Number of data entries.
    ///
    /// Empty buffers are never written to disk. [`NonZeroUsize`] is used to enforce this invariant.
    #[n(1)]
    pub count: NonZeroUsize,
    /// Minimum value recorded in this buffer. Used for segment-level predicate pruning.
    ///
    /// Data is stored via an arbitrary-length [`Vec`] containing raw bytes encoded in
    /// platform-native endianness. Decode according to the [`Buffer`] type described by the schema.
    #[n(2)]
    pub min: Vec<u8>,
    /// Maximum value recorded in this buffer. Used for segment-level predicate pruning.
    ///
    /// Data is stored via an arbitrary-length [`Vec`] containing raw bytes encoded in
    /// platform-native endianness. Decode according to the [`Buffer`] type described by the schema.
    #[n(3)]
    pub max: Vec<u8>,
}

mod number {
    //! A minimal and extensible numeric type **descriptor** for Rust primitives.
    //!
    //! Defining a distinct enum variant for each fixed-width machine primitive type is fragile; as
    //! Rust stabilises new types – such as `f16` – new enum variants will need to be added, which
    //! may break backwards compatibility and binary encoding.
    //!
    //! Instead, this module defines an extensible [`Number`] descriptor to encode arbitrary numeric
    //! types via a combination of 'kind' and 'size' fields.

    use minicbor::{Decode, Encode};
    use serde::{Deserialize, Serialize};
    use std::any::TypeId;

    /// Classification of the numeric primitive type.
    #[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize, Encode, Decode)]
    #[non_exhaustive] // To accommodate the potential stabilisation of additional numeric classes.
    pub(super) enum Kind {
        /// Unsigned integer type.
        #[n(0)]
        UInt,
        /// Signed integer type.
        #[n(1)]
        Int,
        /// Floating point type.
        #[n(2)]
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

/// A minimal dictionary segment **descriptor** that specifies:
///
/// 1. [`Sector`] where the schema segment is located on disk.
/// 2. [`BTreeMap`] of [`Column`] descriptors keyed by name.
///
/// This type does **not** contain the actual schema definition or columnar data buffers; it is a
/// lightweight descriptor for segment discovery and access without holding buffer contents in
/// memory. An on-disk schema segment encodes the schema definition (column names and types) while
/// on-disk data segments contain the columnar buffers.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub(crate) struct Dictionary {
    /// Location of the schema segment including header.
    #[n(0)]
    pub schema: Sector,
    /// Column descriptors keyed by name.
    #[cbor(n(1), skip_if = "BTreeMap::is_empty")]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<String, Column>,
}

impl Dictionary {
    /// Returns a reference to the "key" column descriptor for this dictionary.
    pub fn key(&self) -> &Column {
        // SAFETY: Dictionaries are guaranteed to contain a "key" column at the type tree root.
        // 1. Serializer rejects incompatible type layouts during dictionary initialisation.
        // 2. Deserializer compares the type tree root against the required { key: K, value: V }
        //    layout. Only exact matches are deserialized into Dictionary instances.
        self.columns
            .get("key")
            .expect("Dictionary does not contain a 'key' column")
    }
}

/// A minimal dictionary index **descriptor** that specifies:
///
/// 1. Underlying [`Dictionary`] descriptor.
/// 2. Next available `key` for appending new entries to the dictionary.
///
/// This type does **not** contain the actual dictionary entries; it is a lightweight descriptor for
/// index discovery and access without holding buffer contents in memory. An on-disk schema segment
/// encodes the schema definition (column names and types) while on-disk data segments contain the
/// columnar buffers.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub(crate) struct Index {
    /// Underlying [`Dictionary`] descriptor.
    #[n(0)]
    pub dictionary: Dictionary,
    /// Next available key.
    ///
    /// Data is stored via an arbitrary-length [`Vec`] containing raw bytes encoded in
    /// platform-native endianness. Decode according to the `Key` type described by the schema.
    #[n(1)]
    pub next: Vec<u8>,
}

