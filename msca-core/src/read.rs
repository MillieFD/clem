/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! Data **streaming** interface for [query](crate::query) execution.
//!
//! ---
//!
//! [msca](crate) maximises IO performance by storing on-disk data as columnar [buffers][1]
//! optimised for range-based queries across an arbitrary number of dimensions; however, this
//! underlying format is generally unsuitable for direct manipulation by end-users.
//!
//! This module provides an [iterator-based](Iterator) interface to coordinate the transition from
//! raw binary data into supported rust types; corresponding to **phase 3** of the [read-cycle][2].
//!
//! ### Segment Composition
//!
//! Each [`Dataset`][3] is partitioned into self-describing segments which are immutable once
//! written. Each segment begins with a minimal header consisting of a [`variant`][4] identifier and
//! [`next`](num::NonZeroU64) offset.
//!
//! - [`Schema`][5] segments describe the structure of encoded data.
//! - [`Data`][6] segments carry columnar [buffers][1] for a specified schema.
//!
//! Multimodality and schema evolution are realised by appending additional schema segments. Data
//! storage and file extensibility are realised by appending additional data segments. Format
//! extensibility may be achieved via the introduction of new segment variants in future releases.
//!
//! ### Lazy Zero-Copy Reads
//!
//! Each column is packaged into a lazy zero-copy stream that:
//!
//! 1. Pulls bytes from the retained on-disk buffers.
//! 2. [Deserializes](Deserialize) bytes into the requested type **exactly once**.
//!
//! Streams chain transparently across segments, abstracting away the underlying file structure to
//! provide a seamless interface for end-users.
//!
//! ### Concurrency Model
//!
//! Segments are immutable once written, meaning readers do not require coordination after
//! extracting their list of candidate segments. A concurrent writer appending a new segment must
//! acquire exclusive mutable access to update the [manifest][3] and file [header](io::Header). This
//! temporarily blocks new readers from accessing the manifest but does not affect in-flight reads.
//!
//! This design ensures:
//!
//! - **Multiple readers** can build candidate segment lists from the manifest simultaneously.
//! - **A writer** updating the manifest does not block phase three readers.
//! - **Segment IO** is fully parallel; readers and writers never contend on segment data regions.
//!
//! This module addresses **phase three** of the [read-cycle](io).
//!
//! [1]: crate::manifest::Buffer
//! [2]: crate::io
//! [3]: crate::dataset::Dataset
//! [4]: crate::segment::Variant
//! [5]: crate::schema::Schema
//! [6]: crate::Data

use std::ops::Not;
use std::{iter, num};

use bitvec::order::Lsb0;
use bitvec::slice::BitSlice;

use crate::io::{Deserialize, Deserializer, Error, SizedBuf};
use crate::query;
use crate::schema::number;

/* ------------------------------------------------------------------------------ Public Exports */

/// A **stateful cursor** over paired validity and value sub-buffers for a single [`Column`]; used
/// to [`Deserialize`] optional non-niche items.
pub struct OptBitVec<'d, I>
where
    I: Decode<'d>,
{
    /// Byte [slice][1] over the validity sub-buffer where `true` → [`Some`] and `false` → [`None`].
    ///
    /// [1]: https://doc.rust-lang.org/std/primitive.slice.html
    mask: &'d BitSlice<u8, Lsb0>,
    /// A **stateful reader** over the concatenated data sub-buffer from which [`Some`] items are
    /// [deserialized](Deserialize).
    data: I::Src,
}

impl<'de, I> Deserialize<'de> for OptBitVec<'de, I>
where
    I: Decode<'de>,
    I::Src: Default,
{
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        let mask = SizedBuf::deserialize(src)?.deserialize_into()?;
        let data = match src.is_empty() {
            true => I::Src::default(),
            false => SizedBuf::deserialize(src)?.deserialize_into()?,
        };
        Ok(Self { mask, data })
    }
}

/// A **stateful cursor** over paired offset and value sub-buffers for a single [`Column`]; used to
/// [`Deserialize`](Deserialize) [unsized][1] items.
///
/// [1]: https://doc.rust-lang.org/reference/dynamically-sized-types.html
pub struct Seq<'a> {
    /// Byte [slice][1] over the `ends` sub-buffer yielding one `u64` cumulative end offset for each
    /// [`Some`] or [`u64::MAX`] for [`None`].
    ///
    /// [1]: https://doc.rust-lang.org/std/primitive.slice.html
    ends: &'a [u8],
    /// Concatenated data sub-buffer from which [`Some`] items are [deserialized](Deserialize).
    data: &'a [u8],
}

impl<'de> Deserialize<'de> for Seq<'de> {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        let ends = SizedBuf::deserialize(src)?.deserialize_into()?;
        let data = match src.is_empty() {
            true => <&[u8]>::default(),
            false => SizedBuf::deserialize(src)?.deserialize_into()?,
        };
        Ok(Self { ends, data })
    }
}

/// A **deserialisation primitive** for [niche][1] [optional](Option) items; a compiler optimisation
/// technique that leverages unused bit patterns (niches) to represent additional states without
/// increasing the [size](Sized) of the type.
///
/// ### Data Layout
///
/// The corresponding [`OptInSitu`][2] accumulator inlines [`Some`] and [`None`] items directly into
/// a single data buffer for supported niche types; no validity mask is required. The alternative
/// [`OptBitVec`][3] accumulator provides a fallback implementation for non-niche types.
///
/// ### Guidance
///
/// Implementers are advised to use niche-optimised types when possible to improve storage density
/// and random access read performance.
///
/// [1]: https://doc.rust-lang.org/std/option/index.html#representation
/// [2]: crate::accumulate::OptInSitu
/// [3]: crate::accumulate::OptBitVec
pub struct OptInSitu<'a>(&'a [u8]);

impl<'de> Deserialize<'de> for OptInSitu<'de> {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        // NOTE: consumes the whole source; a buffer sector must frame the slice externally
        <&[u8]>::deserialize(src).map(OptInSitu)
    }
}

/* --------------------------------------------------------------------- Decoder Trait Definition */

/// A **stateful data source** used to construct a lazy deserializing [`Iterator`].
pub trait Decoder<'a, I> {
    /// Return an [`Iterator`] that lazily [deserializes](Deserialize) items from the on-disk bytes.
    ///
    /// ### Errors
    ///
    /// - An iterator construction [`Error`] surfaces **eagerly** through the outer [`Result`].
    /// - Per-item deserialisation errors surface **lazily** on [`next`](Iterator::next).
    #[rustfmt::skip] // single line where clause improves readability
    fn iter(self) -> Result<impl Iterator<Item = Result<I, Error>> + 'a, Error> where Self: Sized;

    /// [`Deserialize`] exactly one item from [`self`](Self).
    ///
    /// ### Implementation
    ///
    /// The default implementation constructs a temporary [`iterator`](Self::iter) and
    /// [pulls](Iterator::next) the **first** item. Implementation overrides are implemented for
    /// specific stateless decoders that can deserialize one item directly.
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Truncated`] if the source holds no item.
    ///
    /// [1]: crate::manifest::Buffer::Compact
    fn one(self) -> Result<I, Error>
    where
        Self: Sized,
    {
        self.iter()?.next().transpose()?.ok_or(Error::Truncated { expected: 1, actual: usize::MIN })
    }
}

/* ----------------------------------------------------------------- Decoder Trait Implementation */

impl<'a, I> Decoder<'a, I> for &'a [u8]
where
    I: for<'de> Deserialize<'de, Ok = I> + 'a,
{
    fn iter(mut self) -> Result<impl Iterator<Item = Result<I, Error>> + 'a, Error> {
        let iter = iter::from_fn(move || match self.is_empty() {
            false => self.deserialize_into().into(),
            true => None,
        });
        Ok(iter)
    }

    fn one(mut self) -> Result<I, Error> {
        // NOTE: deserialize fixed-width item directly from slice (override improves performance)
        self.deserialize_into()
    }
}

impl<'a> Decoder<'a, bool> for &'a BitSlice<u8, Lsb0> {
    fn iter(self) -> Result<impl Iterator<Item = Result<bool, Error>> + 'a, Error> {
        let iter = self.iter().by_vals().map(Ok);
        Ok(iter)
    }

    fn one(self) -> Result<bool, Error> {
        // NOTE: deserialize first bit directly from slice (override improves performance)
        self.first().map(|b| *b).ok_or(Error::Truncated { expected: 1, actual: usize::MIN })
    }
}

impl<'a, I> Decoder<'a, Option<I>> for OptBitVec<'a, I>
where
    I: Decode<'a> + 'a,
{
    fn iter(self) -> Result<impl Iterator<Item = Result<Option<I>, Error>> + 'a, Error> {
        let mut mask = self.mask.iter().by_vals();
        let mut data = self.data.iter()?;
        let iter = iter::from_fn(move || match mask.next()? {
            true => data.next()?.map(Some).into(),
            false => Ok(None).into(),
        });
        Ok(iter)
    }

    fn one(self) -> Result<Option<I>, Error> {
        // NOTE: data sub-buffer untouched if mask is false (override improves performance)
        match Decoder::<bool>::one(self.mask)? {
            true => self.data.one().map(Some),
            false => Ok(None),
        }
    }
}

impl<'a, I> Decoder<'a, Option<I>> for OptInSitu<'a>
where
    I: 'a,
    Option<I>: for<'de> Deserialize<'de, Ok = Option<I>>,
{
    fn iter(self) -> Result<impl Iterator<Item = Result<Option<I>, Error>> + 'a, Error> {
        let mut data = self.0;
        let iter = iter::from_fn(move || match data.is_empty() {
            false => Option::<I>::deserialize(&mut data).into(),
            true => None,
        });
        Ok(iter)
    }

    fn one(mut self) -> Result<Option<I>, Error> {
        // NOTE: deserialize fixed-width item directly from slice (override improves performance)
        Option::<I>::deserialize(&mut self.0)
    }
}

impl<'a> Decoder<'a, &'a str> for Seq<'a> {
    fn iter(self) -> Result<impl Iterator<Item = Result<&'a str, Error>> + 'a, Error> {
        let (mut ends, mut data) = (self.ends, self.data);
        let mut start = usize::MIN;
        let iter = iter::from_fn(move || {
            ends.is_empty().not().then(|| {
                let end: usize = u64::deserialize(&mut ends)?.try_into()?;
                let len = end.checked_sub(start).ok_or(number::Error::Zero)?;
                data.split_at_checked(len)
                    .ok_or_else(|| Error::Truncated { expected: len, actual: data.len() })
                    .and_then(|src| {
                        data = src.1;
                        start = end;
                        Ok(src.0)
                    })
                    .map(str::from_utf8)?
                    .map_err(Error::from)
            })
        });
        Ok(iter)
    }
}

impl<'a> Decoder<'a, String> for Seq<'a>
where
    Self: Decoder<'a, &'a str>,
{
    fn iter(self) -> Result<impl Iterator<Item = Result<String, Error>> + 'a, Error> {
        let iter = Decoder::<&'a str>::iter(self)?.map(|item| item.map(str::to_owned));
        Ok(iter)
    }
}

impl<'a> Decoder<'a, Option<String>> for Seq<'a> {
    fn iter(self) -> Result<impl Iterator<Item = Result<Option<String>, Error>> + 'a, Error> {
        let (mut ends, mut data) = (self.ends, self.data);
        let mut start = usize::MIN;
        let iter = iter::from_fn(move || {
            ends.is_empty().not().then(|| {
                let end: usize = match u64::deserialize(&mut ends)? {
                    u64::MAX => return Ok(None), // in-situ niche
                    other => other.try_into()?,
                };
                let len = end.checked_sub(start).ok_or(number::Error::Zero)?;
                data.split_at_checked(len)
                    .ok_or_else(|| Error::Truncated { expected: len, actual: data.len() })
                    .and_then(|src| {
                        data = src.1;
                        start = end;
                        Ok(src.0)
                    })
                    .map(str::from_utf8)?
                    .map(str::to_owned)
                    .map(Some)
                    .map_err(Error::from)
            })
        });
        Ok(iter)
    }
}

impl<'a, I> Decoder<'a, Vec<I>> for Seq<'a>
where
    I: Decode<'a> + 'a,
{
    fn iter(self) -> Result<impl Iterator<Item = Result<Vec<I>, Error>> + 'a, Error> {
        let (mut ends, mut start) = (self.ends, usize::MIN);
        let mut data = I::Src::deserialize(&mut { self.data })?.iter()?;
        let iter = iter::from_fn(move || {
            ends.is_empty().not().then(|| {
                let end: usize = u64::deserialize(&mut ends)?.try_into()?;
                let n = end.checked_sub(start).ok_or(number::Error::Zero)?;
                start = end;
                data.by_ref().take(n).collect()
            })
        });
        Ok(iter)
    }
}

impl<'a, I> Decoder<'a, Option<Vec<I>>> for Seq<'a>
where
    I: Decode<'a> + 'a,
{
    fn iter(self) -> Result<impl Iterator<Item = Result<Option<Vec<I>>, Error>> + 'a, Error> {
        let (mut ends, mut start) = (self.ends, usize::MIN);
        let mut data = I::Src::deserialize(&mut { self.data })?.iter()?;
        let iter = iter::from_fn(move || {
            ends.is_empty().not().then(|| {
                let end: usize = match u64::deserialize(&mut ends)? {
                    u64::MAX => return Ok(None),
                    end => end.try_into()?,
                };
                let n = end.saturating_sub(start);
                start = end;
                let item: Result<Vec<I>, Error> = data.by_ref().take(n).collect();
                Some(item).transpose()
            })
        });
        Ok(iter)
    }
}

/* --------------------------------------------------------------------- Decode Trait Definition */

/// A **data type** that can be lazily [deserialized](Deserialize) from one on-disk [`Column`][1].
///
/// ### Guidance
///
/// Implemented for all supported primitive types. Implementors are advised to [`#derive(Read)`][1]
/// for [`Composite`] types that [zip](Iterator::zip) one sub-stream per field.
// [1]: TODO → add link to msca-derive crate or feature
pub trait Decode<'d>: Sized {
    /// The [stateful data source](Decoder) from which to [`Deserialize`] values of [`Self`].
    type Src: Deserialize<'d, Ok = Self::Src> + Decoder<'d, Self>;
}

/* ----------------------------------------------------------------- Decode Trait Implementation */

impl<'d> Decode<'d> for u8 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for u16 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for u32 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for u64 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for u128 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for i8 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for i16 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for i32 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for i64 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for i128 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for f32 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for f64 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for char {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for num::NonZeroU8 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for num::NonZeroU16 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for num::NonZeroU32 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for num::NonZeroU64 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for num::NonZeroU128 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for num::NonZeroI8 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for num::NonZeroI16 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for num::NonZeroI32 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for num::NonZeroI64 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for num::NonZeroI128 {
    type Src = &'d [u8];
}

impl<'d> Decode<'d> for bool {
    type Src = &'d BitSlice<u8, Lsb0>;
}

impl<'d> Decode<'d> for Option<u8> {
    type Src = OptBitVec<'d, u8>;
}

impl<'d> Decode<'d> for Option<u16> {
    type Src = OptBitVec<'d, u16>;
}

impl<'d> Decode<'d> for Option<u32> {
    type Src = OptBitVec<'d, u32>;
}

impl<'d> Decode<'d> for Option<u64> {
    type Src = OptBitVec<'d, u64>;
}

impl<'d> Decode<'d> for Option<u128> {
    type Src = OptBitVec<'d, u128>;
}

impl<'d> Decode<'d> for Option<i8> {
    type Src = OptBitVec<'d, i8>;
}

impl<'d> Decode<'d> for Option<i16> {
    type Src = OptBitVec<'d, i16>;
}

impl<'d> Decode<'d> for Option<i32> {
    type Src = OptBitVec<'d, i32>;
}

impl<'d> Decode<'d> for Option<i64> {
    type Src = OptBitVec<'d, i64>;
}

impl<'d> Decode<'d> for Option<i128> {
    type Src = OptBitVec<'d, i128>;
}

impl<'d> Decode<'d> for Option<f32> {
    type Src = OptBitVec<'d, f32>;
}

impl<'d> Decode<'d> for Option<f64> {
    type Src = OptBitVec<'d, f64>;
}

impl<'d> Decode<'d> for Option<bool> {
    type Src = OptBitVec<'d, bool>;
}

impl<'d> Decode<'d> for Option<char> {
    type Src = OptInSitu<'d>;
}

impl<'d> Decode<'d> for Option<num::NonZeroU8> {
    type Src = OptInSitu<'d>;
}

impl<'d> Decode<'d> for Option<num::NonZeroU16> {
    type Src = OptInSitu<'d>;
}

impl<'d> Decode<'d> for Option<num::NonZeroU32> {
    type Src = OptInSitu<'d>;
}

impl<'d> Decode<'d> for Option<num::NonZeroU64> {
    type Src = OptInSitu<'d>;
}

impl<'d> Decode<'d> for Option<num::NonZeroU128> {
    type Src = OptInSitu<'d>;
}

impl<'d> Decode<'d> for Option<num::NonZeroI8> {
    type Src = OptInSitu<'d>;
}

impl<'d> Decode<'d> for Option<num::NonZeroI16> {
    type Src = OptInSitu<'d>;
}

impl<'d> Decode<'d> for Option<num::NonZeroI32> {
    type Src = OptInSitu<'d>;
}

impl<'d> Decode<'d> for Option<num::NonZeroI64> {
    type Src = OptInSitu<'d>;
}

impl<'d> Decode<'d> for Option<num::NonZeroI128> {
    type Src = OptInSitu<'d>;
}

impl<'d, I> Decode<'d> for Vec<I>
where
    I: Decode<'d> + 'd,
{
    type Src = Seq<'d>;
}

impl<'d, I> Decode<'d> for Option<Vec<I>>
where
    I: Decode<'d> + 'd,
{
    type Src = Seq<'d>;
}

impl<'d> Decode<'d> for String {
    type Src = Seq<'d>;
}

impl<'d> Decode<'d> for Option<String> {
    type Src = Seq<'d>;
}

impl<'d> Decode<'d> for &'d str {
    type Src = Seq<'d>;
}

/* ------------------------------------------------------------------- Evaluate Trait Definition */

/// A **deserialized item** that can be mapped to an [`Outcome`] using the provided [closure][1].
///
/// [1]: https://doc.rust-lang.org/book/ch13-01-closures.html
pub trait Evaluate<I = Self>: Sized {
    /// Assess `self` using the provided [`filter`](F) and maps:
    ///
    /// - `true` → [`Outcome::Include`]
    /// - `false` → [`Outcome::Exclude`]
    ///
    /// Used to subtractively reduce a [query] result set.
    ///
    /// # Performance
    ///
    /// This function takes an [`Fn`] with a generic input type [`I`] determined at compile time.
    /// The compiler [monomorphises][2] each `evaluate` function call to minimise runtime overhead
    /// and eliminate [`fn`] pointer chasing.
    ///
    /// Refer to the [trait documentation](Self) for more details.
    ///
    /// [1]: https://doc.rust-lang.org/book/ch13-01-closures.html
    /// [2]: https://rustc-dev-guide.rust-lang.org/backend/monomorph.html
    #[rustfmt::skip] // single line where clause improves readability
    fn evaluate<F>(&self, filter: F) -> bool where F: Fn(&I) -> bool;
}

/* --------------------------------------------------------------- Evaluate Trait Implementation */

impl Evaluate for bool {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for char {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for f32 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for f64 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for i8 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for i16 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for i32 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for i64 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for i128 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for u8 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for u16 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for u32 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for u64 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for u128 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for num::NonZeroI8 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for num::NonZeroI16 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for num::NonZeroI32 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for num::NonZeroI64 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for num::NonZeroI128 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for num::NonZeroU8 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for num::NonZeroU16 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for num::NonZeroU32 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for num::NonZeroU64 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for num::NonZeroU128 {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for String {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl Evaluate for &str {
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl<I> Evaluate<Self> for Option<I> {
    /// Assess the **whole** optional item, so a structural test sees an absent item instead of
    /// having it excluded before the predicate runs.
    ///
    /// This is the counterpart to [`Evaluate<I>`](Evaluate), which unwraps to the inner operand;
    /// the two never overlap because they differ in the trait parameter.
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&Self) -> bool,
    {
        filter(self)
    }
}

impl<I> Evaluate<I> for Option<I>
where
    I: Evaluate,
{
    /// Assess the wrapped [`item`](I) using the provided [`filter`](F) if the option is [`Some`].
    ///
    /// ### Excludes None
    ///
    /// The filter has no input – and therefore cannot be executed – if the option is [`None`]. An
    /// absent item carries no operand and therefore cannot satisfy the predicate. Untested items
    /// are **excluded** by default. Use [`IsNone::is_none`][1] to include these items.
    ///
    /// Refer to the [trait documentation](Evaluate) for more details.
    ///
    /// [1]: query::filter::IsNone::is_none
    fn evaluate<F>(&self, filter: F) -> bool
    where
        F: Fn(&I) -> bool,
    {
        self.as_ref().is_some_and(|item| item.evaluate(filter))
    }
}

/* ------------------------------------------------------------------- IsOption Trait Definition */

/// An **optional item** which can be [`Some`] or [`None`].
///
/// Refer to [std::option] for more details.
#[doc(hidden)] // pub required for filter trait bounds; not intended as a stable API
pub trait IsOption {
    /// The item carried when the option is [`Some`].
    type Item;

    /// Returns `true` if the option is [`Some`].
    fn is_some(&self) -> bool;

    /// Returns `true` if the option is [`None`].
    fn is_none(&self) -> bool;

    /// Consume the option and yield the item it carries, if any.
    ///
    /// A structural filter narrows the item type past the option, so the stream carries
    /// [`Item`](Self::Item) and an absent slot becomes [`Outcome::Absent`].
    fn item(self) -> Option<Self::Item>;
}

/* --------------------------------------------------------------- IsOption Trait Implementation */

impl<I> IsOption for Option<I> {
    type Item = I;

    fn is_some(&self) -> bool {
        self.is_some()
    }

    fn is_none(&self) -> bool {
        self.is_none()
    }

    fn item(self) -> Option<I> {
        self
    }
}

/* ----------------------------------------------------------------- Unfiltered Trait Definition */

/// The default source for an item read with no filter attached: every column it names, combined.
///
/// An unfiltered read is a conjunction of every column with nothing attached, so it goes through
/// the same tree as a filtered one rather than assembling the streams by a second route.
///
/// Implemented by `#[derive(Read)]`, which alone knows the field list. The reader is [`Read::Src`],
/// which is what that associated type means for a composite: the source items deserialize from.
pub trait Unfiltered<'q>: Read<Src<'q>: Iterator<Item = Outcome<Self>>> + Sized {
    /// Open every column of the [`Query`](query::Query), combine them, and assemble the reader.
    ///
    /// The combination is handed to [`Composite::new`], so an unfiltered read reaches the streams
    /// through the same tree a filtered one does rather than by a second route.
    ///
    /// ### Errors
    ///
    /// Returns [`query::Error`] if a named column is missing or its type is incompatible.
    fn unfiltered(query: &'q query::Query<'q>) -> Result<Self::Src<'q>, query::Error>;
}

/* ------------------------------------------------------------------ Composite Trait Definition */

/// A **composite reader** assembled from multiple [column iterators](Reader::iter).
///
/// This trait is not implemented for any [std] types; it exists solely for complex algebraic data
/// types. Implementations are generated by the `#[derive(Read)]` procedural macro.
///
/// ### State Machine
///
/// Each composite [`Reader`] is used to reconstruct instances of a corresponding external [`Read`]
/// type, which is linked to the reader via [`Read::Src`]. Each composite reader exists as a
/// transient state machine, holding one [column iterator](Reader::iter) for each field of the
/// `Read` type.
// TODO → basic rust example showing external struct and corresponding composite reader
///
/// The composite reader pulls one [`Outcome`] from each iterator:
///
/// - Returns [`Outcome::Error`] if **any** column returns an [`Error`].
/// - Excludes the item if **any** column returns [`Outcome::Exclude`].
/// - Reconstructs the item if **all** columns return [`Outcome::Include`].
///
/// Column types are verified against the on-disk schema **exactly once**; enabling subsequent
/// iteration and reconstruction to progress fearlessly without additional runtime overhead.
#[doc(hidden)] // Reachable through the #[derive(Read)] macro; not intended as a stable API
pub trait Composite<'d, S>: Sized {
    /// The reader assembling [`Self`] from one stream per column.
    type Reader: Iterator<Item = Outcome<Self>>;

    /// Assemble the reader from `src`.
    ///
    /// ### Errors
    ///
    /// Returns [`query::Error`] if a required column is missing or its type is incompatible.
    fn new(src: S) -> Result<Self::Reader, query::Error>;
}

/* --------------------------------------------------------------------------------------- Tests */

#[cfg(test)]
mod tests {
    use super::*;

    /* ---------------------------------------------------------------------------- Shared State */

    /// Append a length-prefixed, 64-bit-aligned sized region to `out`, mirroring the on-disk
    /// [`SizedBuf`](crate::io) layout independently of the production serializer.
    fn sized(payload: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        out.extend_from_slice(payload);
        let pad = (8 - (payload.len() & 7)) & 7;
        out.resize(out.len() + pad, 0);
    }

    /* ------------------------------------------------------------------------------ Unit Tests */

    /// [`Seq::deserialize`] splits the composite body into its raw `ends` and `data` sub-buffers.
    #[test]
    fn seq_deserialize_splits_ends_and_data() {
        let mut buf = Vec::new();
        sized(&3u64.to_le_bytes(), &mut buf); // one cumulative end offset
        sized(b"abc", &mut buf);
        let mut src = buf.as_slice();
        let seq = Seq::deserialize(&mut src).expect("Deserialize failed");
        assert_eq!(seq.ends, &3u64.to_le_bytes());
        assert_eq!(seq.data, b"abc");
    }

    /// [`OptBitVec::deserialize`] splits the composite body into its validity `mask` and value
    /// `data` sub-buffers; the mask marks rows 0 and 2 as present.
    #[test]
    fn opt_bit_vec_deserialize_splits_mask_and_data() {
        let mut buf = Vec::new();
        sized(&[0b0000_0101], &mut buf);
        let data = [1u32.to_le_bytes(), 3u32.to_le_bytes()].concat();
        sized(&data, &mut buf);
        let mut src = buf.as_slice();
        let opt = OptBitVec::<u32>::deserialize(&mut src).expect("Deserialize failed");
        assert!(opt.mask[0] && !opt.mask[1] && opt.mask[2]);
        assert_eq!(opt.data, data.as_slice());
    }

    /// [`OptBitVec::deserialize`] accepts the omitted value sub-buffer written by an all-[`None`]
    /// column; the exhausted source yields an empty data cursor.
    #[test]
    fn opt_bit_vec_deserialize_accepts_omitted_data() {
        let mut buf = Vec::new();
        sized(&[0b0000_0000], &mut buf);
        let mut src = buf.as_slice();
        let opt = OptBitVec::<u32>::deserialize(&mut src).expect("Deserialize failed");
        assert!(!opt.mask[0]);
        assert!(opt.data.is_empty());
    }

    /// [`Seq::deserialize`] accepts the omitted data sub-buffer written by an all-empty-row column;
    /// the exhausted source yields an empty data cursor.
    #[test]
    fn seq_deserialize_accepts_omitted_data() {
        let mut buf = Vec::new();
        sized(&0u64.to_le_bytes(), &mut buf); // one zero-length row end offset
        let mut src = buf.as_slice();
        let seq = Seq::deserialize(&mut src).expect("Deserialize failed");
        assert_eq!(seq.ends, &0u64.to_le_bytes());
        assert!(seq.data.is_empty());
    }

    /// [`From`] lifts a decode result onto an [`Outcome`] at the column boundary.
    #[test]
    fn outcome_lifts_a_decode_result() {
        let include = Outcome::from(Ok::<u32, Error>(7));
        let error = Outcome::from(Err::<u32, Error>(Error::Utf8));
        assert!(matches!(include, Outcome::Include(7)));
        assert!(matches!(error, Outcome::Error(..)));
    }

    /// [`included`](Squash::included) drops every [`Exclude`](Outcome::Exclude) and surfaces every
    /// [`Error`](Outcome::Error) as [`Err`], preserving the order of the surviving items.
    #[test]
    fn resolution_yields_the_included_items_in_order() {
        let stream = [
            Outcome::Exclude(1u32), // leading exclusions never reach the caller
            Outcome::Include(2),
            Outcome::Exclude(3), // consecutive exclusions are skipped in one pull
            Outcome::Exclude(4),
            Outcome::Error(Error::Utf8),
            Outcome::Include(5),
            Outcome::Exclude(6), // a trailing exclusion exhausts the stream
        ];
        let items: Vec<Result<u32, Error>> = stream.into_iter().included().collect();
        assert_eq!(items.len(), 3); // four of the seven slots were excluded
        assert!(matches!(items[0], Ok(2)));
        assert!(matches!(items[1], Err(Error::Utf8)));
        assert!(matches!(items[2], Ok(5)));
    }

    /// [`excluded`](Squash::excluded) is the mirror: it drops every [`Include`](Outcome::Include)
    /// and surfaces the same [`Error`](Outcome::Error) entries, so the two partition the stream.
    #[test]
    fn exclusion_yields_the_rejected_items_in_order() {
        let stream = [
            Outcome::Exclude(1u32),
            Outcome::Include(2), // an inclusion never reaches the caller
            Outcome::Error(Error::Utf8),
            Outcome::Include(3),
            Outcome::Exclude(4),
        ];
        let items: Vec<Result<u32, Error>> = stream.into_iter().excluded().collect();
        assert_eq!(items.len(), 3); // two of the five slots were included
        assert!(matches!(items[0], Ok(1)));
        assert!(matches!(items[1], Err(Error::Utf8)));
        assert!(matches!(items[2], Ok(4)));
    }

    /// A stream of nothing but [`Exclude`](Outcome::Exclude) yields no items rather than looping.
    #[test]
    fn resolution_drains_a_wholly_excluded_stream() {
        let stream = [Outcome::Exclude(1u32), Outcome::Exclude(2)];
        let count = stream.into_iter().included().count();
        assert_eq!(count, usize::MIN);
    }

    /// [`Evaluate`] tests a plain item against its own operand; an [`Option`] defers to its inner
    /// operand and rejects an absent [`None`], which carries no operand to test.
    #[test]
    fn evaluate_projects_the_operand() {
        let plain_in = 7u32.evaluate(|op| *op == 7);
        let plain_out = 7u32.evaluate(|op| *op == 8);
        let some_in = Some(7u32).evaluate(|op: &u32| *op == 7);
        let some_out = Some(7u32).evaluate(|op: &u32| *op == 8);
        let absent = None::<u32>.evaluate(|op: &u32| *op == 7);
        assert!(plain_in && some_in);
        assert!(!(plain_out || some_out || absent));
    }

    /// [`keep`](Outcome::keep) wraps the [`Evaluate`] verdict; every other outcome passes through
    /// untouched, so an item a preceding filter rejected is never re-included.
    #[test]
    fn keep_wraps_the_verdict_and_passes_other_outcomes_through() {
        let included = Outcome::Include(7u32).keep(|op: &u32| *op == 7);
        let rejected = Outcome::Include(7u32).keep(|op: &u32| *op == 8);
        let excluded = Outcome::Exclude(7u32).keep(|op: &u32| *op == 7);
        let absent = Outcome::<u32>::Absent.keep(|op: &u32| *op == 7);
        assert!(matches!(included, Outcome::Include(7)));
        assert!(matches!(rejected, Outcome::Exclude(7)));
        assert!(matches!(excluded, Outcome::Exclude(7)));
        assert!(matches!(absent, Outcome::Absent));
    }

    /// A failing predicate becomes [`Error`](Outcome::Error), reported through the same channel as
    /// [`IsOption`] reports structural presence for the `is_some` / `is_none` gate.
    #[test]
    fn is_option_reports_presence() {
        assert!(Some(7u32).is_some());
        assert!(!None::<u32>.is_some());
    }
}
