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

