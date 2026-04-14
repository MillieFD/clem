/*
Project: clem
GitHub: https://github.com/MillieFD/clem

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

use std::convert::Infallible;
use std::fmt;
use std::num::TryFromIntError;

/* ----------------------------------------------------------------------------- Public Exports */

/// Errors returned by [`clem`](crate).
///
/// Enum variants cover various granular error cases that may arise when working with datasets,
/// schemas, or column operations. Users should consider handling errors explicitly wherever
/// possible to provide meaningful error messages and recovery actions.
///
/// ### Implementation
///
/// This enum is `#[non_exhaustive]` meaning additional variants may be added in future versions.
/// Implementers are advised to include a wildcard arm `_` to account for potential additions.
#[non_exhaustive]
#[derive(Debug)]
pub enum Error {
    /// Underlying [`std::io::Error`] from the file backing the [`Dataset`].
    Io(std::io::Error),
    /// CBOR encode or decode failure for a manifest or schema payload.
    Cbor,
    /// On-disk data is truncated or structurally invalid.
    Corrupt,
    /// File magic bytes did not match the expected `clem` signature.
    Magic,
    /// File version not recognised by this build of [`clem`](crate).
    Version(u8),
    /// Unsupported [`serde`] construct encountered while building a schema.
    Schema(&'static str),
    /// Type does not exactly match the existing on-disk schema.
    SchemaMismatch,
    /// Type cannot project from the existing on-disk schema.
    SchemaProjection,
    /// Referenced schema name not present in the manifest.
    SchemaMissing(String),
    /// Referenced column name not present in the schema.
    Column(String),
    /// Dictionary type collision in the [`Dataset`] cache.
    DictionaryKind,
    /// Duplicate key on dictionary push.
    DuplicateKey,
    /// Row count exceeded `u64::MAX` in a single segment.
    Count,
    /// Manifest decode failed and segment-walk rebuild did not succeed.
    Manifest,
    /// Filter applied to an incompatible column.
    Filter(&'static str),
    /// Join leg type or column mismatch.
    Join(&'static str),
}

/* ----------------------------------------------------------------------- Trait Implementations */

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Cbor => f.write_str("cbor encode or decode failed"),
            Self::Corrupt => f.write_str("corrupt file data"),
            Self::Magic => f.write_str("invalid magic bytes"),
            Self::Version(v) => write!(f, "unrecognised version: {v}"),
            Self::Schema(s) => write!(f, "schema build: {s}"),
            Self::SchemaMismatch => f.write_str("schema mismatch"),
            Self::SchemaProjection => f.write_str("schema cannot project"),
            Self::SchemaMissing(s) => write!(f, "schema not found: {s}"),
            Self::Column(c) => write!(f, "column not found: {c}"),
            Self::DictionaryKind => f.write_str("dictionary type collision"),
            Self::DuplicateKey => f.write_str("duplicate dictionary key"),
            Self::Count => f.write_str("row count overflow"),
            Self::Manifest => f.write_str("manifest decode failed"),
            Self::Filter(s) => write!(f, "filter: {s}"),
            Self::Join(s) => write!(f, "join: {s}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
