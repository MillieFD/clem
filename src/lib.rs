/*
Project: clem
GitHub: https://github.com/MillieFD/clem

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! Domain-agnostic high-throughput storage for n-dimensional analytical data.
//!
//! ---
//!
//! `clem` maximises read and write performance by separating the data lifecycle into two phases:
//!
//! 1. **In-memory** accumulator optimised for high-throughput ingestion.
//! 2. **On-disk** columnar archive optimised for range-based querying across arbitrary dimensions.
//!
//! `clem` provides an extensible backend which can be adapted to suit a variety of scientific
//! applications. Implementers benefit from a minimal high-performance core library which can be
//! further enhanced via domain-specific optimisations.
//!
//! Files are organised as a sequence of self-describing **segments** followed by a **manifest** and
//! optional **metadata**. See the [`FORMAT.md`](FORMAT.md) specification for more details.
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

mod dataset;
mod dictionary;
mod error;
mod io;
mod manifest;
mod query;
mod schema;
mod segment;
mod stream;
mod substream;

/* ----------------------------------------------------------------------------- Private Imports */

use minicbor::{Decode, Encode};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::num::{NonZeroU8, NonZeroU16, NonZeroU32, NonZeroU64, NonZeroU128, NonZeroUsize};

/* ------------------------------------------------------------------------------ Public Exports */

pub use self::dataset::Dataset;
pub use self::dictionary::{Dictionary, Index};
pub use self::error::Error;
pub use self::query::{Cursor, Query};
pub use self::stream::Stream;
pub use self::substream::SubStream;

/// A contiguous byte range within the [`clem`](crate) file.
///
/// Implementers must [`Copy`] into an owned type when mutability is required e.g. for downstream
/// data processing.
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Encode, Decode)]
#[cbor(tag(102))]
pub struct Sector {
    /// Byte offset to the start of the sector.
    #[n(0)]
    pub offset: usize,
    /// Total length of the sector in bytes.
    #[n(1)]
    pub length: NonZeroUsize,
}

impl Sector {
    pub fn new<A, B>(offset: A, length: B) -> Result<Self, Error>
    where
        A: TryInto<usize>,
        B: TryInto<NonZeroUsize>,
        Error: From<A::Error> + From<B::Error>,
    {
        Ok(Self {
            offset: offset.try_into()?,
            length: length.try_into()?,
        })
    }
}

/// Marker trait for unsigned [`non-zero`](core::num::nonzero::NonZero) integer types.
pub trait NonZeroUInt: Copy + Ord + Sized {
    /// A constant representing the multiplicative identity element for the implementing type.
    ///
    /// Multiplication by this constant should leave any compatible instance of the type unchanged.
    ///
    /// ### Example
    ///
    /// ```rust
    /// fn noop<T: NonZeroUInt>(value: T) {
    ///    assert_eq!(T::ONE * value, value);
    ///    assert_eq!(value * T::ONE, value);
    /// }
    /// ```
    ///
    /// [`Self::ONE`] represents the minimum permissible value of the implementing type.
    const ONE: Self;
}

/* ----------------------------------------------------------------------- Trait Implementations */

impl Ord for Sector {
    fn cmp(&self, other: &Self) -> Ordering {
        self.offset.cmp(&other.offset)
    }
}

impl NonZeroUInt for NonZeroU8 {
    const ONE: Self = Self::MIN;
}

impl NonZeroUInt for NonZeroU16 {
    const ONE: Self = Self::MIN;
}

impl NonZeroUInt for NonZeroU32 {
    const ONE: Self = Self::MIN;
}

impl NonZeroUInt for NonZeroU64 {
    const ONE: Self = Self::MIN;
}

impl NonZeroUInt for NonZeroU128 {
    const ONE: Self = Self::MIN;
}

/* --------------------------------------------------------------------------------------- Tests */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sector_ord() {
        let hi = Sector::new(200, 16).expect("Sector::new failed for hi");
        let lo = Sector::new(100, 16).expect("Sector::new failed for lo");
        assert!(hi > lo);
        assert!(lo < hi);
    }

    #[test]
    fn sector_eq() {
        let short = Sector::new(100, 16).expect("Sector::new failed for short");
        let long = Sector::new(100, 32).expect("Sector::new failed for long");
        assert_ne!(short, long);
    }

    #[test]
    fn sector_copy() {
        let a = Sector::new(10, 5).expect("Sector::new failed");
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn non_zero_uint_ord() {
        assert!(NonZeroU8::MIN < NonZeroU8::MAX);
        assert!(NonZeroU16::MIN < NonZeroU16::MAX);
        assert!(NonZeroU32::MIN < NonZeroU32::MAX);
        assert!(NonZeroU64::MIN < NonZeroU64::MAX);
        assert!(NonZeroU128::MIN < NonZeroU128::MAX);
    }

    #[test]
    fn non_zero_uint_one() {
        assert_eq!(NonZeroU8::ONE.get() * NonZeroU8::new(2).unwrap().get(), 2);
        assert_eq!(NonZeroU16::ONE.get() * NonZeroU16::new(2).unwrap().get(), 2);
        assert_eq!(NonZeroU32::ONE.get() * NonZeroU32::new(2).unwrap().get(), 2);
        assert_eq!(NonZeroU64::ONE.get() * NonZeroU64::new(2).unwrap().get(), 2);
    }

    #[test]
    fn niche_optimisation() {
        assert_eq!(size_of::<Option<NonZeroU8>>(), size_of::<NonZeroU8>());
        assert_eq!(size_of::<Option<NonZeroU16>>(), size_of::<NonZeroU16>());
        assert_eq!(size_of::<Option<NonZeroU32>>(), size_of::<NonZeroU32>());
        assert_eq!(size_of::<Option<NonZeroU64>>(), size_of::<NonZeroU64>());
        assert_eq!(size_of::<Option<NonZeroU128>>(), size_of::<NonZeroU128>());
    }
}
