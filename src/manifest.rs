/*
Project: clem
GitHub: https://github.com/MillieFD/clem

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! A [`manifest`][1] footer lists file [`segments`][2] by type. Data segments are grouped by schema
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
use std::num::{NonZeroU64, NonZeroUsize};

/* ------------------------------------------------------------------------------ Public Exports */

/// Manifest of file segments and accompanying metadata for random access and predicate pruning.
/// See the module-level documentation for details.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Encode, Decode)]
#[cbor(tag(100))]
pub(crate) struct Manifest {
    /// Schema segments keyed by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[n(0)]
    pub schemas: BTreeMap<String, Schema>,
    /// Dictionaries keyed by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[n(1)]
    pub dictionaries: BTreeMap<String, DictEntry>,
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
    /// Column descriptors keyed by name.
    #[cbor(n(1), skip_if = "BTreeMap::is_empty")]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<String, Column>,
}

/// A minimal column **descriptor** that wraps a list of [`Buffer`] descriptors.
///
/// This type does **not** contain the actual buffer data; it is a lightweight descriptor for
/// column discovery and access without holding buffer contents in memory. Data is stored via
/// one or more on-disk data segments, each of which contains a buffer for this column.
///
/// [`Vec`] order in-memory is **not** guaranteed to reflect [`Sector`] order on-disk.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub(crate) struct Column {
    /// List of [`Buffer`] descriptors for this column across all data segments.
    #[cbor(n(0), skip_if = "Vec::is_empty")]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buffers: Vec<Buffer>,
}

