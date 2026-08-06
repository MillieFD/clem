/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! Domain-agnostic high-throughput storage for n-dimensional analytical data.
//!
//! ---
//!
//! `msca` maximises read and write performance by separating the data lifecycle into two phases:
//!
//! 1. **In-memory** accumulator optimised for high-throughput ingestion.
//! 2. **On-disk** columnar archive optimised for range-based querying across arbitrary dimensions.
//!
//! `msca` provides an extensible backend which can be adapted to suit a variety of scientific
//! applications. Implementers benefit from a minimal high-performance core library which can be
//! further enhanced via domain-specific optimisations.
//!
//! Files are organised as a sequence of self-describing **segments** followed by a **manifest** and
//! optional **metadata**. Refer to `on-disk-format.md` for more details.
//!
//! ### Sector vs Segment
//!
//! Each `Segment` is a self-describing contiguous file region written to disk. In addition to
//! conventional `data` segments – which encode columnar data buffers – format extensibility is
//! achieved via segment variants. Each segment type is identified via a `variant: u8` ID in the
//! segment header. A `length` field allows sequential readers to skip to the next segment (no
//! segment footer required).
//!
//! A [`Sector`] is the minimal abstraction: a contiguous byte range within a file, described by a
//! starting [`offset`](Sector::offset) and [`length`](Sector::length) in bytes. A sector can
//! describe any contiguous file region, from a single columnar buffer to an entire segment.

// NOTE: Required to resolve msca::something in tests and doc code
extern crate self as msca;

/* ------------------------------------------------------------------------------ Public Exports */

pub use msca_core::{
    Accumulate,
    Accumulator,
    Adjunct,
    Align,
    Bin,
    Column,
    Columns,
    Combine,
    Composite,
    Conjunct,
    Data,
    Dataset,
    Describe,
    Deserialize,
    Disjunct,
    Error,
    Evaluate,
    IsOption,
    Join,
    Mmap,
    Outcome,
    Query,
    Read,
    Reader,
    Schema,
    Sector,
    Serialize,
    SizedBuf,
    Unfiltered,
    accumulate,
    binary,
    io,
    manifest,
    query,
    read,
    schema,
};

/* ----------------------------------------------------------------------- Feature Gated Exports */

#[cfg(feature = "derive")]
pub use msca_derive::{Data, Read};
