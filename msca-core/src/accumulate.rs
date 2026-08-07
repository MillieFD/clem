/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! This module provides composable in-memory data accumulation primitives, each mapped to a
//! separate on-disk space optimisation strategy:
//!
//! - [`OptInSitu`] → Data buffer with [validity](Option) via niche.
//! - [`OptBitVec`] → Data buffer with [validity](Option) [mask](BitVec).
//! - [`Seq`] → Data buffer with [offset](NonZeroU64) metadata.
//! - [`OptSeq`] → [`Seq`] with [offset](NonZeroU64) and [validity](Option) metadata.
//! - [`Flatten`] → Collapses nested [`Option`] layers.
//! - [`Buffer`] → State machine to determine the appropriate encoding strategy.
//!
//! Each accumulator type implements the [`Accumulate`] trait, which defines a shared interface for
//! handling in-memory value accumulation.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::{self, Debug};
use std::iter;
use std::num::*;
use std::ops::Range;

use bitvec::field::BitField;
use bitvec::vec::BitVec;
use minicbor::{CborLen, Decode, Encode};

use crate::io::{Checksum, HEADER, Register, SizedBuf};
use crate::manifest::{self, Column, Manifest};
use crate::number::Error;
use crate::schema::{self, BitMatch, Unfold, size_of_opt};
use crate::segment::{Align, Header, Segment, Variant};
use crate::{Data, Sector, io};

/// Shorthand type-erased [`Iterator`] over mutable [`Column`] descriptors.
// NOTE: Deterministic runtime order via BTreeMap; #[derive] ensures identical compile time order.
pub type Columns<'a> = dyn Iterator<Item = &'a mut Column> + 'a;

/// An **in-memory staging buffer** used to build data segments for the specified [`Schema`].
///
/// ### Segment Composition
///
/// Each [msca](crate) file is partitioned into self-describing segments which are immutable once
/// written. Each segment begins with a minimal header consisting of a [`variant`](Variant) ID and
/// [`length`](NonZeroU64).
///
/// - [`Schema`][1] segments describe the structure of encoded data.
/// - [`Data`][2] segments carry columnar buffers for a specified schema instance.
///
/// Each data segment is associated with a **single** schema segment. This association is primarily
/// included for data integrity and crash recovery; the optimised read path filters data segments by
/// schema using the `manifest`.
///
/// ```text
/// data-segment
/// ├─ header
/// │  ├─ variant: u8
/// │  └─ size: NonZeroU64
/// ├─ metadata
/// │  ├─ schema: NonZeroU64
/// │  ├─ count: NonZeroU64
/// │  └─ alignment padding
/// ├─ 1st buffer
/// ⋮
/// ├─ Nth buffer
/// └─ checksum: u64
/// ```
///
/// The [`Schema`][1] maps each **platform-agnostic** primitive [`Type`][3] to a contiguous buffer;
/// providing essential context for buffer deserialization. Each `Accumulator` holds a [`Sector`]
/// for the corresponding schema which is written to disk within each data segment header. All
/// columns contain an equal number of rows indicated by `count` in the segment header.
///
/// Refer to the [schema module documentation](schema) for more details.
///
/// ### Concurrent Producers
///
/// Initialise a new accumulator via [`Dataset::schema`][4] which:
///
/// - Constructs a [`Schema`][2] for the specified [`Item`](I) type.
/// - Eagerly writes a schema segment to disk if unregistered.
/// - Reuses the existing schema segment if already registered.
/// - Returns a new empty accumulator.
///
/// Use [`Clone`] to initialise a new empty accumulator for the same [`Schema`][2] and [`Item`](I)
/// type. Users should prefer to clone an existing valid accumulator to bypass schema lookup and
/// [`Type`][3] verification compared to [`Dataset::schema`][4]. Each clone is independent and
/// starts empty.
///
/// 1. Register the [`Schema`][1] once via [`Dataset::schema`][4] to initialise an accumulator.
/// 2. [`Clone`] one accumulator for each worker thread.
/// 3. Each thread accumulates data independently.
/// 4. Commit each accumulator sequentially via [`Dataset::write`][5].
///
/// Refer to the [write-cycle documentation](io) for more details.
///
/// [1]: schema::Schema
/// [2]: crate::Data
/// [3]: schema::Type
/// [4]: crate::Dataset::schema
/// [5]: crate::Dataset::write
pub struct Accumulator<I>
where
    I: Data,
{
    /// The monomorphized composite accumulator for [`I`], named through [`Data::Acc`] so no trait
    /// object erases it: the whole accumulator tree stays inlineable.
    pub data: I::Acc,
    /// [Name](String) of the corresponding [`Schema`][1] registered in the [`Manifest`].
    ///
    /// [1]: crate::Schema
    pub(crate) name: String,
    /// [`Sector`] of the corresponding [`Schema`](crate::Schema) segment describing the structure
    /// of accumulated data.
    pub schema: Sector,
}

impl<I> Accumulator<I>
where
    I: Data,
{
    /// Size of the data segment [`Header`] and metadata in bytes.
    ///
    /// Refer to the [`Accumulator`] documentation for more details regarding segment layout.
    pub(crate) const HEADER: usize = Header::SIZE
        + size_of::<NonZeroU64>() // corresponding schema segment offset
        + size_of::<NonZeroU64>(); // number of items in this segment (count)

    /// Returns the exact number of bytes required to encode the data segment body for `self`;
    /// including segment metadata and metadata→body [alignment](Align) bytes.
    ///
    /// ### ⚠️ Caution
    ///
    /// This function **cannot** predict the exact on-disk segment size. The number of
    /// header→metadata alignment bytes is determined by the absolute [`Segment`] offset. This
    /// function also excludes the segment [`Header`] and [`Checksum`]. Refer to [`Segment::wrap`]
    /// for an accurate on-disk representation.
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Zero`] if the size overflows `u64` or the [`Accumulator`] is empty.
    fn size(&self) -> Result<NonZeroU64, Error> {
        let meta = { size_of::<NonZeroU64>() + size_of::<NonZeroU64>() } as u64;
        self.data.size()?.align()?.checked_add(meta).and_then(NonZeroU64::new).ok_or(Error::Zero)
    }
}

/// Returns a new empty [`Accumulator`] for the same [`Schema`][1] and [`Item`](I) type.
///
/// Prefer to [`Clone`] an existing valid accumulator to bypass schema lookup and [`Type`][2]
/// verification compared to [`Dataset::schema`][3].
///
/// [1]: manifest::Schema
/// [2]: schema::Type
/// [3]: crate::Dataset::schema
impl<I> Clone for Accumulator<I>
where
    I: Data,
{
    fn clone(&self) -> Self {
        // NOTE: each clone starts empty due to I::Acc::default
        Self {
            data: I::Acc::default(),
            name: self.name.clone(),
            schema: self.schema,
        }
    }
}

impl<I> Debug for Accumulator<I>
where
    I: Data,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Accumulator")
            .field("item", &std::any::type_name::<I>())
            .field("name", &self.name)
            .field("schema", &self.schema)
            .finish()
    }
}

/* --------------------------------------------------------------------------- Data Accumulators */

/// Data accumulator for [optional](Option) values with niche optimisation; a compiler optimisation
/// technique that leverages unused bit patterns (niches) to represent additional states without
/// increasing the [size](size_of) of the type.
///
/// ### Data Layout
///
/// [`OptInSitu`] encodes [`Some`] and [`None`] values directly in a single data buffer for
/// supported niche types; no validity mask is required.
///
/// [`OptBitVec`] provides a fallback implementation for non-niche types.
///
/// ### Guidance
///
/// Implementors are advised to use niche-optimised types when possible to improve storage
/// efficiency and random read performance.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
pub struct OptInSitu<T> {
    /// Contiguous payload encoding [`Some`] and [`None`] values directly.
    #[cbor(n(0), skip_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub data: Vec<Option<T>>,
}

impl<T> Default for OptInSitu<T> {
    fn default() -> Self {
        // NOTE: cannot use generic static assertion; must be verified per monomorphisation.
        debug_assert_eq!(size_of::<T>(), size_of_opt::<T>(), "Use OptBitVec");
        Self { data: Vec::new() }
    }
}

/// Data accumulator for [optional](Option) values without niche optimisation.
///
/// ### Data Layout
///
/// [`OptBitVec`] encodes [validity](Option) and [value](I) separately for non-niche types:
///
/// 1. A packed [`BitVec`] encodes [`Some`] as `true`.
/// 2. A contiguous data buffer encodes only [`Some`] values.
///
/// [`None`] entries append no data; the validity mask alone records their position. This design
/// improves storage density at the expense of index-based random access. Users are encouraged to
/// stream data via the [`Query`](crate::Query) interface.
///
/// ### Guidance
///
/// The sibling [`OptInSitu`] type encodes [`Some`] and [`None`] values directly in a single data
/// buffer for supported niche types; no validity mask required. Implementors are advised to use
/// niche-optimised types when possible to improve storage efficiency and random read performance.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct OptBitVec<I>
where
    I: Unfold,
{
    /// Validity mask where `true → `[`Some`] and `false → `[`None`].
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BitVec::is_empty")
    )]
    pub mask: BitVec,
    /// Contiguous payload of [`Some`] items only; [`None`] items append no data.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Accumulate::is_empty")
    )]
    pub data: I::RawAcc,
}

// NOTE: #[derive(Default)] would impose I: Default trait bound which complicates the proc macro
impl<I> Default for OptBitVec<I>
where
    I: Unfold,
{
    fn default() -> Self {
        Self {
            mask: BitVec::default(),
            data: I::RawAcc::default(),
        }
    }
}

impl<I> OptBitVec<I>
where
    I: Unfold,
{
    /// Byte offset of the `data` sub-buffer within the serialized [`OptBitVec`] body; excludes the
    /// `mask` sub-buffer and data size prefix.
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Zero`] if the offset overflows `u64`.
    fn origin(&self) -> Result<u64, Error> {
        let mask = SizedBuf::new(&self.mask).size()?.get();
        mask.checked_add(SizedBuf::<I::RawAcc>::PREFIX).ok_or(Error::Zero)
    }
}

/// Data **accumulator** for [unsized][1] values.
///
/// ### Data Layout
///
/// It is not possible to predetermine the on-disk space required by each instance of an unsized
/// type; there is no guarantee that two [`Vec<I>`] contain the same number of elements.
/// The [msca](crate) engine therefore unfolds unsized types into:
///
/// 1. Columnar `ends` region describing boundaries.
/// 2. Contiguous `data` region encoding items.
///
/// This design ensures **O(1) random access** and avoids per-element pointer chasing. Sequential
/// scans across the contained [items](I) remain linear; leveraging columnar optimisations for SIMD
/// and prefetch.
///
/// Each offset records one **zero-based** cumulative end per row, with `0` corresponding to the
/// start of the concatenated `data` region. Item `i` spans `ends[i - 1] → ends[i]` with an
/// implicit leading `0` if not otherwise specified. The offset count therefore equals the item
/// count recorded in the segment header.
///
/// ```text
/// ends: [3, 6, 6, 8]
/// data:  [a, b, c, d, e, f, g, h]
/// ```
///
/// The serialized on-disk example above (four items) is deserialized into the memory representation
/// below. Implementers can specify which type to use for offset storage based on the number of
/// expected elements.
///
/// ```text
/// Row 0 → data[..3] → "abc"
/// Row 1 → data[3..6] → "def"
/// Row 2 → data[6..6] → "" (empty)
/// Row 3 → data[6..8] → "gh"
/// ```
///
/// Nested unsized types use **multiple offset layers** alongside a **single data region**. This
/// composable design preserves the performance advantages associated with contiguous item storage;
/// namely predictable vectorised traversal. Scanning performance across the contiguous inner `data`
/// region is unaffected by deep nesting. The inner ends buffer is aligned in memory order of
/// traversal to improve cache locality during nested iteration and reduce TLB misses.
///
/// ```text
/// inner ends
/// outer ends
/// data
/// ```
///
/// [1]: https://doc.rust-lang.org/reference/dynamically-sized-types.html
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Seq<I>
where
    I: Unfold,
{
    /// Cumulative ends.
    ///
    /// Offset `n` marks the exclusive end of item `n` and the inclusive start of item `n + 1`.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    // TODO Allow users to specify the offset type based on the number of expected elements.
    pub ends: Vec<u64>,
    /// Flattened and concatenated [item](I) accumulator.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Accumulate::is_empty")
    )]
    pub data: I::RawAcc,
}

// NOTE: #[derive(Default)] would impose I: Default trait bound which complicates the API
impl<I> Default for Seq<I>
where
    I: Unfold,
{
    fn default() -> Self {
        Self {
            ends: Vec::new(),
            data: I::RawAcc::default(),
        }
    }
}

impl<I> Seq<I>
where
    I: Unfold + Clone + 'static,
{
    /// Returns an [`Iterator`] over [`Range`] instances that each describe the location of one item
    /// within the concatenated `data` sub-buffer, where item `n` spans `ends[n - 1] → ends[n]` with
    /// an implicit leading `0`.
    ///
    /// Refer to the [unsized accumulator documentation](Seq) for more details.
    #[allow(unused)]
    fn bounds(&self) -> impl Iterator<Item = Range<u64>> {
        let ubs = self.ends.iter().copied();
        let lbs = ubs.clone();
        iter::once(u64::MIN).chain(lbs).zip(ubs).map(|b| b.0..b.1)
    }
}

/// Data **accumulator** for [optional](Option) [unsized][1] items.
///
/// ### Data Layout
///
/// It is not possible to predetermine the disk space required by each instance of an unsized type;
/// there is no guarantee that two [`Vec<I>`] contain the same number of elements. The [msca](crate)
/// engine therefore unfolds unsized types into:
///
/// 1. Columnar `ends` region describing boundaries.
/// 2. Contiguous `data` region encoding items.
///
/// [`OptSeq`] encodes validity in the `ends` buffer without an auxiliary bitmap. [`None`] items are
/// marked using a [`u64::MAX`] sentinel offset and append no data.
///
/// Refer to the [documentation](Seq) on non-optional unsized type accumulation for more details.
///
/// [1]: https://doc.rust-lang.org/reference/dynamically-sized-types.html
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct OptSeq<I>
where
    I: Unfold,
{
    /// Cumulative end offsets.
    ///
    /// Offset `n` marks the exclusive end of item `n` and the inclusive start of item `n + 1`.
    /// [`u64::MAX`] marks a [`None`] item (no data appended).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub ends: Vec<u64>,
    /// Flattened and concatenated [item](I) accumulator; only [`Some`] items contribute entries.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Accumulate::is_empty")
    )]
    pub data: I::RawAcc,
}

impl<I> Default for OptSeq<I>
where
    I: Unfold,
{
    fn default() -> Self {
        Self {
            ends: Vec::new(),
            data: I::RawAcc::default(),
        }
    }
}

impl<I> OptSeq<I>
where
    I: Unfold,
{
    /// Returns an [`Iterator`] over [`Range`] instances that each describe the location of one item
    /// within the concatenated `data` sub-buffer, where item `n` spans `ends[n - 1] → ends[n]` with
    /// an implicit leading `0`.
    ///
    /// Refer to the [optional unsized accumulator](OptSeq) documentation for more details.
    #[allow(unused)]
    fn bounds(&self) -> impl Iterator<Item = Range<u64>> {
        let ubs = self.ends.iter().copied().filter(|&end| end != u64::MAX);
        let lbs = ubs.clone();
        iter::once(u64::MIN).chain(lbs).zip(ubs).map(|b| b.0..b.1)
    }
}

/// Stateless type-level wrapper that flattens nested types on [`push`](Accumulate::push). All
/// storage lives in the inner accumulator.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
pub struct Flatten<I>(#[n(0)] pub I);

/// A **state machine** used to build [data buffers](manifest::Buffer) for the specified [`Column`].
///
/// ### Buffer Composition
///
/// Real-world applications often require the inclusion of columns with infrequently altered items.
/// It is possible for a column to contain only **one** repeated item across an entire data segment.
/// Instead of repeatedly encoding identical items, [msca](crate) defaults to a **compact buffer**
/// representation to improve storage density.
// TODO → Add link to on-disk-format.md for more information.
///
/// ##### 1. Empty
///
/// Each column begins in the [`Empty`](Buffer::Empty) state, which is never written to disk. If an
/// empty [`Buffer`] is encountered during the [write-cycle](crate::io), the entire data segment is
/// discarded. This behaviour may change in future releases; using absent buffers to encode a
/// type-dependent [`Default`].
///
/// ##### 2. Compact
///
/// The column transitions to the [`Compact`](Buffer::Compact) state when the first [`item`](I) is
/// [pushed](Accumulate::push). This state holds the item directly and tracks the number of
/// accumulated repetitions to coalesce homogenous runs. All subsequently [accumulated](Accumulate)
/// items are compared against this value.
///
/// ##### 3. Many
///
/// The column transitions to the [`Many`](Buffer::Many) state when a [pushed](Accumulate::push)
/// item is not [bit-identical](BitMatch::eq) to the [accumulated](Accumulate) item; materialising
/// the required [accumulator](`I::RawAcc`) with the specified number of repeated items.
///
/// Buffer `Empty → Compact → Many` variant escalation is unidirectional; a `Compact` buffer can
/// never return to the `Empty` state.
///
/// ### Guidance
///
/// Implementers are encouraged to use a [`Bin`](crate::Bin) segment for genuinely constant data
/// that never changes across the file lifetime. This improves storage efficiency by eliminating an
/// unnecessary column from the schema.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Buffer<I>
where
    I: Unfold,
{
    /// A buffer which does not contain any data; never written to disk.
    Empty,
    /// A buffer containing a single [`item`](I) repeated `count` times; used to coalesce homogenous
    /// runs into an efficient on-disk representation to improve storage density.
    Compact {
        /// The single repeated item.
        item: I,
        /// The number of accumulated repetitions.
        count: u64,
    },
    /// A buffer containing many different [items](I) collected into an [accumulator](I::RawAcc).
    Many(I::RawAcc),
}

impl<I> Buffer<I>
where
    I: Unfold + Clone,
{
    /// Constructor for [`Buffer::Many`] that materialises the required [accumulator](`I::RawAcc`)
    /// containing the specified number of [repeated](iter::repeat_n) identical [items](I) followed
    /// by the one new item.
    fn upgrade(item: &I, count: &u64, new: I) -> Self {
        let one = iter::once(new);
        let acc = iter::repeat_n(item.clone(), *count as usize).chain(one).collect();
        Self::Many(acc)
    }
}

/// Constructor for [`Buffer::Empty`].
impl<I> Default for Buffer<I>
where
    I: Unfold,
{
    fn default() -> Self {
        Self::Empty
    }
}

/// Constructor for [`Buffer::Compact`] wrapping the specified [`item`](I) with a count of one.
impl<I> From<I> for Buffer<I>
where
    I: Unfold,
{
    fn from(item: I) -> Self {
        Self::Compact { item, count: 1 }
    }
}

/* ----------------------------------------------------------------- Accumulate Trait Definition */

/// An in-memory **data accumulator** that ingests [items](I) of the specified [`Type`][1] and
/// [serializes](Serialize) into an optimised on-disk format.
///
/// [1]: schema::Type
pub trait Accumulate<I> {
    /// Append one [`Item`](I) to the [accumulator](Self)
    fn push(&mut self, item: I);

    /// Reinitialise the [accumulator](Self) without writing to disk. All data is permanently lost.
    ///
    /// Note that this method may not affect the allocated capacity of the underlying storage.
    fn discard(&mut self);

    /// Returns `true` if the [accumulator](Self) contains no data.
    fn is_empty(&self) -> bool;

    /// Returns the number of accumulated [`items`](I).
    fn count(&self) -> u64;
}

/* ------------------------------------------------------------- Accumulate Trait Implementation */

impl<I> Accumulate<I> for Accumulator<I>
where
    I: Data,
{
    fn push(&mut self, item: I) {
        self.data.push(item);
    }

    fn discard(&mut self) {
        self.data.discard();
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn count(&self) -> u64 {
        self.data.count()
    }
}

impl Accumulate<bool> for BitVec {
    fn push(&mut self, item: bool) {
        BitVec::push(self, item);
    }

    fn discard(&mut self) {
        BitVec::clear(self);
    }

    fn is_empty(&self) -> bool {
        BitVec::is_empty(self)
    }

    fn count(&self) -> u64 {
        BitVec::len(self) as u64
    }
}

impl<I> Accumulate<I> for Vec<I>
where
    I: BitMatch + Copy + PartialOrd + Serialize + Unfold + 'static,
{
    fn push(&mut self, item: I) {
        Vec::push(self, item);
    }

    fn discard(&mut self) {
        Vec::clear(self);
    }

    fn is_empty(&self) -> bool {
        Vec::is_empty(self)
    }

    fn count(&self) -> u64 {
        Vec::len(self) as u64
    }
}

impl<I> Accumulate<Option<I>> for OptInSitu<I>
where
    Option<I>: Serialize,
    I: BitMatch + Copy + PartialOrd + Unfold + 'static,
{
    fn push(&mut self, item: Option<I>) {
        self.data.push(item);
    }

    fn discard(&mut self) {
        self.data.discard();
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn count(&self) -> u64 {
        self.data.count()
    }
}

impl<I> Accumulate<Option<I>> for OptBitVec<I>
where
    I: Unfold + 'static,
{
    fn push(&mut self, item: Option<I>) {
        if let Some(i) = item {
            self.mask.push(true);
            self.data.push(i);
        } else {
            // NOTE: contiguous payload of Some items only; None items append no data.
            self.mask.push(false);
        }
    }

    fn discard(&mut self) {
        self.mask.discard();
        self.data.discard();
    }

    fn is_empty(&self) -> bool {
        self.mask.is_empty() && self.data.is_empty()
    }

    fn count(&self) -> u64 {
        self.mask.len() as u64
    }
}

impl<I> Accumulate<Vec<I>> for Seq<I>
where
    I: Unfold + 'static,
{
    fn push(&mut self, item: Vec<I>) {
        let size = item.len() as u64;
        let next = self.ends.last().copied().unwrap_or(u64::MIN).saturating_add(size);
        item.into_iter().for_each(|i| self.data.push(i));
        self.ends.push(next);
    }

    fn discard(&mut self) {
        self.ends.discard();
        self.data.discard();
    }

    fn is_empty(&self) -> bool {
        self.ends.is_empty() && self.data.is_empty()
    }

    fn count(&self) -> u64 {
        self.ends.len() as u64
    }
}

impl Accumulate<String> for Seq<u8> {
    fn push(&mut self, item: String) {
        let bytes = item.into_bytes();
        self.push(bytes);
    }

    fn discard(&mut self) {
        Accumulate::<Vec<u8>>::discard(self);
    }

    fn is_empty(&self) -> bool {
        Accumulate::<Vec<u8>>::is_empty(self)
    }

    fn count(&self) -> u64 {
        Accumulate::<Vec<u8>>::count(self)
    }
}

impl<I> Accumulate<Option<Vec<I>>> for OptSeq<I>
where
    I: Unfold,
{
    fn push(&mut self, item: Option<Vec<I>>) {
        if let Some(i) = item {
            let next = self
                .ends
                .iter()
                .rev()
                .find(|&o| o != &u64::MAX)
                .copied()
                .unwrap_or(u64::MIN)
                .saturating_add(i.len() as u64);
            i.into_iter().for_each(|x| self.data.push(x));
            self.ends.push(next);
        } else {
            // NOTE: contiguous payload of Some items only; None items append no data.
            self.ends.push(u64::MAX);
        }
    }

    fn discard(&mut self) {
        self.ends.discard();
        self.data.discard();
    }

    fn is_empty(&self) -> bool {
        self.ends.is_empty() && self.data.is_empty()
    }

    fn count(&self) -> u64 {
        self.ends.len() as u64
    }
}

impl Accumulate<Option<String>> for OptSeq<u8> {
    fn push(&mut self, item: Option<String>) {
        let bytes = item.map(String::into_bytes);
        self.push(bytes);
    }

    fn discard(&mut self) {
        Accumulate::<Option<Vec<u8>>>::discard(self);
    }

    fn is_empty(&self) -> bool {
        Accumulate::<Option<Vec<u8>>>::is_empty(self)
    }

    fn count(&self) -> u64 {
        Accumulate::<Option<Vec<u8>>>::count(self)
    }
}

impl<A, B> Accumulate<Option<Option<B>>> for Flatten<A>
where
    A: Accumulate<Option<B>> + Default + Serialize<Buffer = Vec<u8>> + 'static,
{
    fn push(&mut self, item: Option<Option<B>>) {
        self.0.push(item.flatten());
    }

    fn discard(&mut self) {
        self.0.discard();
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn count(&self) -> u64 {
        self.0.count()
    }
}

impl<I> Accumulate<I> for Buffer<I>
where
    I: BitMatch + Clone + Unfold,
{
    fn push(&mut self, new: I) {
        match self {
            Self::Empty => *self = new.into(),
            Self::Compact { item, count } if new.eq(item) => *count += 1,
            Self::Compact { item, count } => *self = Self::upgrade(item, count, new),
            Self::Many(acc) => acc.push(new),
        };
    }

    fn discard(&mut self) {
        *self = Self::default();
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Empty => true,
            Self::Compact { .. } => false,
            Self::Many(acc) => acc.is_empty(),
        }
    }

    fn count(&self) -> u64 {
        match self {
            Self::Empty => u64::MIN,
            Self::Compact { count, .. } => *count,
            Self::Many(acc) => acc.count(),
        }
    }
}

/* ------------------------------------------------------------------- Describe Trait Definition */

/// A type-erasable **column accumulator** that can be walked for [`manifest::Buffer`] descriptors.
///
/// This trait is implemented by the top-level per-column [`Buffer`] state machine. Implementations
/// are also generated for external [`Composite`][1] types. Bare [staging buffers](Unfold::RawAcc)
/// cannot be walked for descriptors.
///
/// ### Descriptor
///
/// `Describe` **walks** an [accumulator](Accumulate) and registers one descriptor per [`Column`];
/// the corresponding [`Descriptor`] trait **produces** per-buffer descriptors.
///
/// - Describe → one-to-many (walk)
/// - Descriptor → one-to-one (production)
///
/// [`Buffer`] implements **both** traits; walking the **one** contained accumulator to register the
/// **one** produced descriptor. Generated composite accumulators contain one independent
/// sub-accumulator per field and implement `Describe` **only**, threading the walk through each
/// field. The in-memory staging buffers implement `Descriptor` **only**.
///
/// [1]: crate::read::Composite
pub trait Describe<I>: Accumulate<I> + Serialize {
    /// Generates one [`Buffer`](manifest::Buffer) descriptor per [`Column`] describing the
    /// [accumulated](Accumulate) data. Each descriptor is appended to the corresponding
    /// [`Manifest`] column entry.
    ///
    /// Returns the next available offset for subsequent buffers.
    ///
    /// ### Errors
    ///
    /// - [`Error::NotFound`][1] if the walk exhausts before every column is described.
    /// - [`Error::Number`][2] if an offset overflows `u64` or the item count is zero.
    ///
    /// [1]: schema::Error::NotFound
    /// [2]: schema::Error::Number
    fn buffers(&self, offset: u64, columns: &mut Columns) -> Result<u64, schema::Error>;
}

/* --------------------------------------------------------------- Describe Trait Implementation */

impl<I> Describe<I> for Buffer<I>
where
    I: BitMatch + Clone + Unfold + Send + Sync + 'static,
{
    fn buffers(&self, offset: u64, columns: &mut Columns) -> Result<u64, schema::Error> {
        if let Some(column) = columns.next() {
            let sector = Sector {
                offset: offset.checked_add(SizedBuf::<I>::PREFIX).ok_or(Error::Zero)?,
                size: self.size()?,
            };
            let count = self.count().try_into().map_err(Error::from)?;
            let buf = self.describe(sector, count)?;
            // Buffers are ordered by sector, monotonic in write order, so the set position is the
            // segment ordinal; no key is stored on disk.
            column.buffers.insert(buf);
            sector.next().ok_or(Error::Zero)?.align().map_err(Into::into)
        } else {
            schema::Error::NotFound.into() // expected column is not present
        }
    }
}

/* ----------------------------------------------------------------- Descriptor Trait Definition */

/// A type-erasable **column accumulator** that produces [`manifest::Buffer`] descriptors.
///
/// This trait is implemented by the in-memory [staging buffers](Unfold::RawAcc) and the top-level
/// per-column [`Buffer`] state machine. Generated [`Composite`][1] accumulators contain independent
/// per-field sub-accumulators and cannot therefore be described by a single [`manifest::Buffer`]
/// descriptor.
///
/// ### Describe
///
/// `Descriptor` **produces** per-buffer descriptors. The corresponding [`Describe`] trait **walks**
/// an [accumulator](Accumulate) and registers one descriptor per [`Column`].
///
/// - Descriptor → one-to-one (production)
/// - Describe → one-to-many (walk)
///
/// [`Buffer`] implements **both** traits; walking the **one** contained accumulator to register the
/// **one** produced descriptor. Generated composite accumulators contain one independent
/// sub-accumulator per field and implement `Describe` **only**, threading the walk through each
/// field. The in-memory staging buffers implement `Descriptor` **only**.
///
/// [1]: crate::read::Composite
#[doc(hidden)] // Reachable through the Unfold accumulator bounds; not intended as a stable API
pub trait Descriptor {
    /// Construct one [`manifest::Buffer`] descriptor recording the accumulated data.
    ///
    /// A homogeneous [`Compact`](Buffer::Compact) buffer emits a corresponding [`Compact`][2]
    /// descriptor and is never asked to self-describe.
    ///
    /// [1]: https://doc.rust-lang.org/reference/dynamically-sized-types.html
    /// [2]: manifest::Buffer::Compact
    fn describe(&self, buffer: Sector, count: NonZeroU64) -> Result<manifest::Buffer, Error>;
}

/* ------------------------------------------------------------- Descriptor Trait Implementation */

impl<I> Descriptor for Vec<I>
where
    I: PartialOrd + Copy,
{
    fn describe(&self, buffer: Sector, count: NonZeroU64) -> Result<manifest::Buffer, Error> {
        self.detail(buffer, count)
    }
}

impl Descriptor for BitVec {
    /// [`Detailed`][1] descriptor statistics are meaningless for `bool`: [`Buffer::Basic`][2]
    /// implies that both `true`/`max` and `false`/`min` items were accumulated.
    ///
    /// Refer to the [trait documentation](Descriptor::describe) for more information.
    ///
    /// [1]: manifest::Buffer::Detailed
    /// [2]: manifest::Buffer::Basic
    fn describe(&self, buffer: Sector, count: NonZeroU64) -> Result<manifest::Buffer, Error> {
        Ok(manifest::Buffer::Basic { buffer, count })
    }
}

impl<I> Descriptor for OptInSitu<I>
where
    I: PartialOrd + Copy,
{
    fn describe(&self, buffer: Sector, count: NonZeroU64) -> Result<manifest::Buffer, Error> {
        self.detail(buffer, count)
    }
}

impl<I> Descriptor for OptBitVec<I>
where
    I: Unfold,
    I::RawAcc: MinMax,
{
    fn describe(&self, buffer: Sector, count: NonZeroU64) -> Result<manifest::Buffer, Error> {
        self.detail(buffer, count)
    }
}

impl Descriptor for OptBitVec<bool> {
    /// [`Detailed`][1] descriptor statistics are meaningless for `bool`: [`Buffer::Basic`][2]
    /// implies that both `true`/`max` and `false`/`min` items were accumulated.
    ///
    /// Refer to the [trait documentation](Descriptor::describe) for more information.
    ///
    /// [1]: manifest::Buffer::Detailed
    /// [2]: manifest::Buffer::Basic
    fn describe(&self, buffer: Sector, count: NonZeroU64) -> Result<manifest::Buffer, Error> {
        Ok(manifest::Buffer::Basic { buffer, count })
    }
}

impl<I> Descriptor for Seq<I>
where
    I: Unfold,
{
    /// [Unsized][1] items do not currently support [`Detailed`][2] descriptor statistics.
    ///
    /// Refer to the [trait documentation](Descriptor::describe) for more information.
    ///
    /// [1]: https://doc.rust-lang.org/reference/dynamically-sized-types.html
    /// [2]: manifest::Buffer::Detailed
    fn describe(&self, buffer: Sector, count: NonZeroU64) -> Result<manifest::Buffer, Error> {
        Ok(manifest::Buffer::Basic { buffer, count })
    }
}

impl<I> Descriptor for OptSeq<I>
where
    I: Unfold,
{
    /// [Unsized][1] items do not currently support [`Detailed`][2] descriptor statistics.
    ///
    /// Refer to the [trait documentation](Descriptor::describe) for more information.
    ///
    /// [1]: https://doc.rust-lang.org/reference/dynamically-sized-types.html
    /// [2]: manifest::Buffer::Detailed
    fn describe(&self, buffer: Sector, count: NonZeroU64) -> Result<manifest::Buffer, Error> {
        Ok(manifest::Buffer::Basic { buffer, count })
    }
}

impl<A> Descriptor for Flatten<A>
where
    A: Descriptor,
{
    fn describe(&self, buffer: Sector, count: NonZeroU64) -> Result<manifest::Buffer, Error> {
        self.0.describe(buffer, count)
    }
}

impl<I> Descriptor for Buffer<I>
where
    I: Unfold,
{
    fn describe(&self, buffer: Sector, count: NonZeroU64) -> Result<manifest::Buffer, Error> {
        match self {
            Buffer::Empty => Error::Zero.into(),
            Buffer::Compact { .. } => Ok(manifest::Buffer::Compact { buffer, count }),
            Buffer::Many(acc) => acc.describe(buffer, count),
        }
    }
}

/* -------------------------------------------------------------------- Extreme Trait Definition */

/// An **in-memory buffer** that can locate the [minimum](Ordering::min) or [maximum](Ordering::max)
/// item within serialized data.
///
/// [Accumulators](Accumulate) that implement this trait are eligible for the [`Detailed`][1]
/// manifest descriptor.
///
/// [1]: manifest::Buffer::Detailed
#[doc(hidden)] // Reachable through the Unfold accumulator bounds; not intended as a stable API
pub trait MinMax {
    /// Returns a [`Sector`] spanning the single minimum or maximum item, or [`None`] if no
    /// accumulated item satisfies the requested [`Ordering`] at **runtime** e.g. an all-none
    /// [optional](Option) column. The sector offset is determined relative to the start of the
    /// serialized byte [slice][1].
    ///
    /// [1]: https://doc.rust-lang.org/std/primitive.slice.html
    fn find(&self, ord: Ordering) -> Option<Sector>;

    /// Construct a [`Detailed`][1] descriptor containing the [minimum](Ordering::min) and
    /// [maximum](Ordering::max) items returned by [`find`](MinMax::find) over the provided on-disk
    /// [`Sector`].
    ///
    /// The found sectors are resolved [`relative`](Sector::relative) to the parent buffer offset
    /// to become **absolute**.
    // TODO → potential data flow rule violation ? try threading the absolute offset into find fn
    /// Returns a fallback [`Basic`][2] descriptor if minimum and maximum items cannot be found.
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Zero`] if a resolved statistic offset overflows `u64`.
    ///
    /// [1]: manifest::Buffer::Detailed
    /// [2]: manifest::Buffer::Basic
    fn detail(&self, buffer: Sector, count: NonZeroU64) -> Result<manifest::Buffer, Error> {
        let min = match self.find(Ordering::Less) {
            None => return Ok(manifest::Buffer::Basic { buffer, count }),
            Some(s) => s.relative(buffer.offset)?,
        };
        let max = match self.find(Ordering::Greater) {
            None => return Ok(manifest::Buffer::Basic { buffer, count }),
            Some(s) => s.relative(buffer.offset)?,
        };
        Ok(manifest::Buffer::Detailed { buffer, count, min, max })
    }
}

/* ---------------------------------------------------------------- Extreme Trait Implementation */

impl<I> MinMax for Vec<I>
where
    I: PartialOrd + Copy,
{
    fn find(&self, ord: Ordering) -> Option<Sector> {
        let cmp = |a: &I, b: &I| a.partial_cmp(b) == Some(ord);
        Sector::find(self.iter().copied().map(Some), size_of::<I>(), cmp)
    }
}

impl<I> MinMax for OptInSitu<I>
where
    I: PartialOrd + Copy,
{
    fn find(&self, ord: Ordering) -> Option<Sector> {
        let cmp = |a: &I, b: &I| a.partial_cmp(b) == Some(ord);
        Sector::find(self.data.iter().copied(), size_of_opt::<I>(), cmp)
    }
}

impl<I> MinMax for OptBitVec<I>
where
    I: Unfold,
    I::RawAcc: MinMax,
{
    fn find(&self, ord: Ordering) -> Option<Sector> {
        // TODO → potential data flow rule violation ? try threading the absolute offset into find
        let origin = self.origin().ok()?;
        self.data.find(ord)?.relative(origin).ok()
    }
}

/* ----------------------------------------------------------- FromIterator Trait Implementation */

// NOTE: iterator collect builds a byte-identical accumulator to sequential Accumulate::push calls

impl<I> FromIterator<Vec<I>> for Seq<I>
where
    I: Unfold + 'static,
{
    fn from_iter<S>(src: S) -> Self
    where
        S: IntoIterator<Item = Vec<I>>,
    {
        let mut acc = Self::default();
        src.into_iter().for_each(|item| acc.push(item));
        acc
    }
}

impl FromIterator<String> for Seq<u8> {
    fn from_iter<S>(src: S) -> Self
    where
        S: IntoIterator<Item = String>,
    {
        let mut acc = Self::default();
        src.into_iter().for_each(|item| acc.push(item));
        acc
    }
}

impl<I> FromIterator<Option<I>> for OptBitVec<I>
where
    I: Unfold + 'static,
{
    fn from_iter<S>(src: S) -> Self
    where
        S: IntoIterator<Item = Option<I>>,
    {
        let mut acc = Self::default();
        src.into_iter().for_each(|item| acc.push(item));
        acc
    }
}

impl<I> FromIterator<Option<I>> for OptInSitu<I>
where
    Option<I>: Serialize,
    I: BitMatch + Copy + PartialOrd + Unfold + 'static,
{
    fn from_iter<S>(src: S) -> Self
    where
        S: IntoIterator<Item = Option<I>>,
    {
        let mut acc = Self::default();
        src.into_iter().for_each(|item| acc.push(item));
        acc
    }
}

impl<I> FromIterator<Option<Vec<I>>> for OptSeq<I>
where
    I: Unfold,
{
    fn from_iter<S>(src: S) -> Self
    where
        S: IntoIterator<Item = Option<Vec<I>>>,
    {
        let mut acc = Self::default();
        src.into_iter().for_each(|item| acc.push(item));
        acc
    }
}

impl FromIterator<Option<String>> for OptSeq<u8> {
    fn from_iter<S>(src: S) -> Self
    where
        S: IntoIterator<Item = Option<String>>,
    {
        let mut acc = Self::default();
        src.into_iter().for_each(|item| acc.push(item));
        acc
    }
}

impl<A, B> FromIterator<Option<Option<B>>> for Flatten<A>
where
    A: Accumulate<Option<B>> + Default + Serialize<Buffer = Vec<u8>> + 'static,
{
    fn from_iter<S>(src: S) -> Self
    where
        S: IntoIterator<Item = Option<Option<B>>>,
    {
        let mut acc = Self::default();
        src.into_iter().for_each(|item| acc.push(item));
        acc
    }
}

/* ------------------------------------------------------------------ Serialize Trait Definition */

/// A **type** that can be serialized into a canonical [`msca`](crate) binary representation for
/// on-disk storage.
#[doc(hidden)]
pub trait Serialize {
    /// The [`Buffer`] type returned by [`Self::serialize`].
    ///
    /// Fixed-size types can specify an appropriate array to leverage stack allocation. Unsized
    /// types should specify a heap-allocated buffer to accommodate dynamic sizing at runtime.
    type Buffer: io::Buffer;

    /// Returns the exact number of bytes required to encode `self`.
    fn size(&self) -> Result<NonZeroU64, Error>;

    /// Serialize `self` into the provided [`Buffer`].
    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error>;

    /// Serialize `self` and return the encoded bytes in a new [`Buffer`].
    fn serialize(&self) -> Result<Self::Buffer, Error>;

    /// Serialize `self` into the provided [`Vec<u8>`] sink.
    ///
    /// This function provides a unified interface for serializing fixed-size (stack) and
    /// dynamic-size (heap) types into a growable [`Buffer`].
    fn extend(&self, mut sink: Vec<u8>) -> Result<Vec<u8>, Error> {
        let buf = &mut sink;
        self.serialize_into(buf)?;
        Ok(sink)
    }
}

/* -------------------------------------------------------------- Serialize Trait Implementation */

/// Blanket [`&I`](I) implementation delegates to the referenced [`item`](I); allowing [`SizedBuf`]
/// to add a **length prefix** and **zero-filled alignment padding** without an intermediate copy.
impl<I> Serialize for &I
where
    I: Serialize + ?Sized,
{
    type Buffer = I::Buffer;

    fn size(&self) -> Result<NonZeroU64, Error> {
        I::size(self)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        I::serialize_into(self, buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        I::serialize(self)
    }

    fn extend(&self, sink: Vec<u8>) -> Result<Vec<u8>, Error> {
        I::extend(self, sink)
    }
}

impl<const N: usize> Serialize for [u8; N] {
    type Buffer = Self;

    fn size(&self) -> Result<NonZeroU64, Error> {
        { N as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        debug_assert!(buf.len() >= N, "actual size < expected size");
        buf[..N].copy_from_slice(self);
        Ok(&mut buf[N..])
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok(*self)
    }
}

impl Serialize for [u8] {
    type Buffer = Vec<u8>;

    fn size(&self) -> Result<NonZeroU64, Error> {
        { self.len() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        let size = self.len();
        debug_assert!(buf.len() >= size, "actual size < buffer size");
        buf[..size].copy_from_slice(self);
        Ok(&mut buf[size..])
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        let buf = self.to_vec();
        Ok(buf)
    }
}

impl Serialize for bool {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        u8::from(*self).serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok([u8::from(*self)])
    }
}

impl Serialize for u8 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.to_le_bytes().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok([*self])
    }
}

impl Serialize for u16 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.to_le_bytes().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok(self.to_le_bytes())
    }
}

impl Serialize for u32 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.to_le_bytes().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok(self.to_le_bytes())
    }
}

impl Serialize for u64 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.to_le_bytes().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok(self.to_le_bytes())
    }
}

impl Serialize for u128 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.to_le_bytes().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok(self.to_le_bytes())
    }
}

impl Serialize for NonZeroU8 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        self.get().size()
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.get().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.get().serialize()
    }
}

impl Serialize for NonZeroU16 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        self.get().size()
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.get().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.get().serialize()
    }
}

impl Serialize for NonZeroU32 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        self.get().size()
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.get().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.get().serialize()
    }
}

impl Serialize for NonZeroU64 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        self.get().size()
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.get().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.get().serialize()
    }
}

impl Serialize for NonZeroU128 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        self.get().size()
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.get().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.get().serialize()
    }
}

impl Serialize for i8 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.to_le_bytes().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok(self.to_le_bytes())
    }
}

impl Serialize for i16 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.to_le_bytes().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok(self.to_le_bytes())
    }
}

impl Serialize for i32 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.to_le_bytes().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok(self.to_le_bytes())
    }
}

impl Serialize for i64 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.to_le_bytes().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok(self.to_le_bytes())
    }
}

impl Serialize for i128 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.to_le_bytes().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok(self.to_le_bytes())
    }
}

impl Serialize for NonZeroI8 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        self.get().size()
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.get().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.get().serialize()
    }
}

impl Serialize for NonZeroI16 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        self.get().size()
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.get().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.get().serialize()
    }
}

impl Serialize for NonZeroI32 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        self.get().size()
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.get().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.get().serialize()
    }
}

impl Serialize for NonZeroI64 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        self.get().size()
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.get().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.get().serialize()
    }
}

impl Serialize for NonZeroI128 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        self.get().size()
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.get().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.get().serialize()
    }
}

impl Serialize for f32 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.to_le_bytes().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok(self.to_le_bytes())
    }
}

impl Serialize for f64 {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.to_le_bytes().serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        Ok(self.to_le_bytes())
    }
}

impl Serialize for char {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        static_assertions::assert_eq_size!(char, u32);
        u32::from(*self).serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        u32::from(*self).serialize()
    }
}

impl Serialize for Option<NonZeroU8> {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        // None writes u8::MIN (outside the valid non-zero range).
        self.map_or(u8::MIN, NonZeroU8::get).serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.map_or(u8::MIN, NonZeroU8::get).serialize()
    }
}

impl Serialize for Option<NonZeroU16> {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        // None writes u16::MIN (outside the valid non-zero range).
        self.map_or(u16::MIN, NonZeroU16::get).serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.map_or(u16::MIN, NonZeroU16::get).serialize()
    }
}

impl Serialize for Option<NonZeroU32> {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        // None writes u32::MIN (outside the valid non-zero range).
        self.map_or(u32::MIN, NonZeroU32::get).serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.map_or(u32::MIN, NonZeroU32::get).serialize()
    }
}

impl Serialize for Option<NonZeroU64> {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        // None writes u64::MIN (outside the valid non-zero range).
        self.map_or(u64::MIN, NonZeroU64::get).serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.map_or(u64::MIN, NonZeroU64::get).serialize()
    }
}

impl Serialize for Option<NonZeroU128> {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        // None writes u128::MIN (outside the valid non-zero range).
        self.map_or(u128::MIN, NonZeroU128::get).serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.map_or(u128::MIN, NonZeroU128::get).serialize()
    }
}

impl Serialize for Option<NonZeroI8> {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        // None writes 0i8 (outside the valid non-zero range).
        self.map_or(0i8, NonZeroI8::get).serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.map_or(0i8, NonZeroI8::get).serialize()
    }
}

impl Serialize for Option<NonZeroI16> {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        // None writes 0i16 (outside the valid non-zero range).
        self.map_or(0i16, NonZeroI16::get).serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.map_or(0i16, NonZeroI16::get).serialize()
    }
}

impl Serialize for Option<NonZeroI32> {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        // None writes 0i32 (outside the valid non-zero range).
        self.map_or(0i32, NonZeroI32::get).serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.map_or(0i32, NonZeroI32::get).serialize()
    }
}

impl Serialize for Option<NonZeroI64> {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        // None writes 0i64 (outside the valid non-zero range).
        self.map_or(0i64, NonZeroI64::get).serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.map_or(0i64, NonZeroI64::get).serialize()
    }
}

impl Serialize for Option<NonZeroI128> {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        // None writes 0i128 (outside the valid non-zero range).
        self.map_or(0i128, NonZeroI128::get).serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.map_or(0i128, NonZeroI128::get).serialize()
    }
}

impl Serialize for Option<char> {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        // None writes u32::MAX (outside the valid scalar range).
        self.map_or(u32::MAX, u32::from).serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.map_or(u32::MAX, u32::from).serialize()
    }
}

impl<T> Serialize for Vec<T>
where
    T: Serialize,
{
    type Buffer = Vec<u8>;

    fn size(&self) -> Result<NonZeroU64, Error> {
        // Recursively sum the sizes of all elements.
        self.iter()
            .try_fold(u64::MIN, |total, element| {
                let size = element.size()?.get();
                total.checked_add(size).ok_or(Error::Zero)
            })
            .map(NonZeroU64::new)
            .transpose()
            .ok_or(Error::Zero)
            .flatten()
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.iter().try_fold(buf, |sink, element| element.serialize_into(sink))
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        use io::Buffer;
        let size = self.size()?.get().try_into()?;
        let buf = vec![0u8; size].serialize_push(self)?;
        // NOTE: cannot use static assertion as size is dependent on runtime data accumulation.
        debug_assert_eq!(buf.len(), size, "actual size ≠ predicted size");
        Ok(buf)
    }
}
impl<K, I> Serialize for BTreeMap<K, I>
where
    K: Serialize + Ord,
    I: Serialize,
{
    type Buffer = Vec<u8>;

    fn size(&self) -> Result<NonZeroU64, Error> {
        // Recursively sum the sizes of all elements.
        self.iter()
            .try_fold(u64::MIN, |total, entry| {
                let size = entry.0.size()?.get() + entry.1.size()?.get();
                total.checked_add(size).ok_or(Error::Zero)
            })
            .map(NonZeroU64::new)
            .transpose()
            .ok_or(Error::Zero)
            .flatten()
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.iter().try_fold(buf, |sink, entry| {
            let sink = entry.0.serialize_into(sink)?;
            entry.1.serialize_into(sink)
        })
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        use io::Buffer;
        let size = self.size()?.get().try_into()?;
        let buf = vec![0u8; size].serialize_push(self)?;
        // NOTE: cannot use static assertion as size is dependent on runtime data accumulation.
        debug_assert_eq!(buf.len(), size, "actual size ≠ predicted size");
        Ok(buf)
    }
}

impl Serialize for BitVec {
    type Buffer = Vec<u8>;

    fn size(&self) -> Result<NonZeroU64, Error> {
        self.chunks(8).len().try_into().map(NonZeroU64::new)?.ok_or(Error::Zero)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        // Intermediate chunks contain 8 bits in Lsb0 order; the final chunk may contain ≤ 8 bits.
        // BitVec::load_le packs each chunk into one u8 in LE order, padding with zeros if the final
        // chunk is shorter than 8 bits. The resulting bytes are pushed into the provided buffer.
        self.chunks(8).try_fold(buf, |sink, bits| bits.load_le::<u8>().serialize_into(sink))
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        use io::Buffer;
        let size = self.size()?.get().try_into()?;
        let buf = vec![0u8; size].serialize_push(self)?;
        // NOTE: cannot use static assertion as size is dependent on runtime data accumulation.
        debug_assert_eq!(buf.len(), size, "actual size ≠ predicted size");
        Ok(buf)
    }
}

impl<T> Serialize for OptInSitu<T>
where
    Vec<Option<T>>: Serialize<Buffer = Vec<u8>>,
{
    type Buffer = Vec<u8>;

    fn size(&self) -> Result<NonZeroU64, Error> {
        self.data.size()
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.data.serialize_into(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.data.serialize()
    }

    fn extend(&self, sink: Vec<u8>) -> Result<Vec<u8>, Error> {
        self.data.extend(sink)
    }
}

impl<T> Serialize for OptBitVec<T>
where
    T: Unfold,
    T::RawAcc: Serialize<Buffer = Vec<u8>>,
{
    type Buffer = Vec<u8>;

    fn size(&self) -> Result<NonZeroU64, Error> {
        let mask = SizedBuf::new(&self.mask).size()?;
        match self.data.is_empty() {
            true => Ok(mask),
            false => SizedBuf::new(&self.data).size()?.checked_add(mask.get()).ok_or(Error::Zero),
        }
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        let buf = SizedBuf::new(&self.mask).serialize_into(buf)?;
        match self.data.is_empty() {
            true => Ok(buf),
            false => SizedBuf::new(&self.data).serialize_into(buf),
        }
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        use io::Buffer;
        let size = self.size()?.get().try_into()?;
        let buf = vec![0u8; size].serialize_push(self)?;
        // NOTE: cannot use static assertion as size is dependent on runtime data accumulation.
        debug_assert_eq!(buf.len(), size, "actual size ≠ predicted size");
        Ok(buf)
    }
}

impl<T> Serialize for Seq<T>
where
    T: Unfold,
    T::RawAcc: Serialize<Buffer = Vec<u8>>,
{
    type Buffer = Vec<u8>;

    fn size(&self) -> Result<NonZeroU64, Error> {
        let ends = SizedBuf::new(&self.ends).size()?;
        match self.data.is_empty() {
            true => Ok(ends),
            false => SizedBuf::new(&self.data).size()?.checked_add(ends.get()).ok_or(Error::Zero),
        }
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        let buf = SizedBuf::new(&self.ends).serialize_into(buf)?;
        match self.data.is_empty() {
            true => Ok(buf),
            false => SizedBuf::new(&self.data).serialize_into(buf),
        }
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        use io::Buffer;
        let size = self.size()?.get().try_into()?;
        let buf = vec![0u8; size].serialize_push(self)?;
        // NOTE: cannot use static assertion as size is dependent on runtime data accumulation.
        debug_assert_eq!(buf.len(), size, "actual size ≠ predicted size");
        Ok(buf)
    }
}

impl<T> Serialize for OptSeq<T>
where
    T: Unfold,
    T::RawAcc: Serialize<Buffer = Vec<u8>>,
{
    type Buffer = Vec<u8>;

    fn size(&self) -> Result<NonZeroU64, Error> {
        let ends = SizedBuf::new(&self.ends).size()?;
        match self.data.is_empty() {
            true => Ok(ends),
            false => SizedBuf::new(&self.data).size()?.checked_add(ends.get()).ok_or(Error::Zero),
        }
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        let buf = SizedBuf::new(&self.ends).serialize_into(buf)?;
        match self.data.is_empty() {
            true => Ok(buf),
            false => SizedBuf::new(&self.data).serialize_into(buf),
        }
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        use io::Buffer;
        let size = self.size()?.get().try_into()?;
        let buf = vec![0u8; size].serialize_push(self)?;
        // NOTE: cannot use static assertion as size is dependent on runtime data accumulation.
        debug_assert_eq!(buf.len(), size, "actual size ≠ predicted size");
        Ok(buf)
    }
}

impl<T> Serialize for Flatten<T>
where
    T: Serialize,
{
    type Buffer = T::Buffer;

    fn size(&self) -> Result<NonZeroU64, Error> {
        self.0.size() // Transparent wrapper
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        self.0.serialize_into(buf) // Transparent wrapper
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        self.0.serialize() // Transparent wrapper
    }

    fn extend(&self, sink: Vec<u8>) -> Result<Vec<u8>, Error> {
        self.0.extend(sink) // Transparent wrapper
    }
}

impl<I> Serialize for Buffer<I>
where
    I: Unfold + Clone,
{
    type Buffer = Vec<u8>;

    fn size(&self) -> Result<NonZeroU64, Error> {
        match self {
            Self::Empty => Error::Zero.into(),
            Self::Compact { item, .. } => I::once(item).size(),
            Self::Many(acc) => acc.size(),
        }
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], Error> {
        match self {
            Self::Empty => Err(Error::Zero),
            Self::Compact { item, .. } => I::once(item).serialize_into(buf),
            Self::Many(acc) => acc.serialize_into(buf),
        }
    }

    fn serialize(&self) -> Result<Self::Buffer, Error> {
        match self {
            Self::Empty => Err(Error::Zero),
            Self::Compact { item, .. } => I::once(item).serialize(),
            Self::Many(acc) => acc.serialize(),
        }
    }
}

impl<I> Segment for Accumulator<I>
where
    I: Data,
{
    const VARIANT: Variant = Variant::Data;

    fn wrap(&self, offset: u64) -> Result<Vec<u8>, Error> {
        use io::Buffer;
        const ADD: u64 = { Header::SIZE + size_of::<u64>() } as u64;
        let pad = offset.checked_add(Self::HEADER as u64).ok_or(Error::Zero)?.pad()?;
        let size = self.size()?.get().checked_add(pad as u64).ok_or(Error::Zero)?;
        let full = size.checked_add(ADD).ok_or(Error::Zero)?.try_into()?;
        let mut buf = vec![u8::MIN; full];
        let rem = buf
            .as_mut_slice()
            .serialize_push(&{ Self::VARIANT as u8 })?
            .serialize_push(&size)?
            .serialize_push(&self.schema.offset)?
            .serialize_push(&self.data.count())?;
        rem[..pad].fill(u8::MIN);
        self.data.serialize_into(&mut rem[pad..])?;
        Self::checksum(&mut buf)?;
        Ok(buf)
    }
}

impl<I> Checksum for Accumulator<I> where I: Data {}

impl<I> Register for Accumulator<I>
where
    I: Data,
{
    type Error = schema::Error;
    type Entry<'m> = &'m mut manifest::Schema;

    fn entry<'m>(&self, m: &'m mut Manifest) -> Result<Self::Entry<'m>, schema::Error> {
        // NOTE: Dataset::schema registers the schema before producing an Accumulator
        m.schemas.get_mut(&self.name).ok_or(schema::Error::NotFound)
    }

    fn register<'a, 'm>(
        self,
        s: &'a Sector,
        e: Self::Entry<'m>,
    ) -> Result<&'a Sector, schema::Error> {
        let offset = s
            .offset
            .checked_add(Self::HEADER as u64)
            .ok_or(Error::Zero)?
            .align()?
            .checked_sub(HEADER as u64)
            .ok_or(Error::Zero)?;
        let mut columns = e.columns.values_mut();
        self.data.buffers(offset, &mut columns)?;
        Ok(s)
    }
}

/* --------------------------------------------------------------------------------------- Tests */

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use memmap2::MmapMut;
    use static_assertions::{assert_impl_all, assert_not_impl_any};

    use super::*;
    use crate::io::Deserializer;
    use crate::schema::Schema;

    /* ---------------------------------------------------------------------------- Shared State */

    /// [describe](Accumulate::describe) `acc` over `count` items at body offset zero, so each
    /// located statistic offset equals its `element index × width`.
    fn describe<A>(acc: &A, count: u64) -> manifest::Buffer
    where
        A: Descriptor + Serialize,
    {
        let length = acc.size().expect("Size failed");
        let sector = Sector::new(u64::MIN, length).expect("Sector::new failed");
        let count = NonZeroU64::new(count).expect("Count is zero");
        acc.describe(sector, count).expect("Describe failed")
    }

    /// The `min` and `max` statistic offsets of a [`Detailed`](manifest::Buffer::Detailed) buffer.
    fn stats(buffer: &manifest::Buffer) -> [u64; 2] {
        let manifest::Buffer::Detailed { min, max, .. } = buffer else {
            panic!("Buffer descriptor is not Detailed")
        };
        [min.offset, max.offset]
    }

    /* ------------------------------------------------------------------------------ Unit Tests */

    /// A leaf [`Buffer`] accumulator is [`Send`] and [`Sync`] for every column layout – a
    /// fixed-width [`u32`], an unsized [`String`], and an optional column – so a worker moves a
    /// cloned accumulator onto its own thread and the write path borrows it across an `await` on a
    /// multi-threaded executor. [`Data::Acc`](crate::Data::Acc) requires both bounds, so the whole
    /// composite tree inherits them.
    #[test]
    fn accumulator_is_send_and_sync() {
        assert_impl_all!(Buffer<u32>: Send, Sync);
        assert_impl_all!(Buffer<String>: Send, Sync);
        assert_impl_all!(Buffer<Option<u32>>: Send, Sync);
    }

    /// [`Accumulate::describe`] locates the extreme items of a populated [`Vec`] by element
    /// position; each statistic spans exactly one serialized item.
    #[test]
    fn vec_locates_extremes() {
        let data: Vec<u32> = vec![3, 1, 2];
        let width = size_of::<u32>() as u64;
        let buffer = describe(&data, 3);
        assert_eq!(stats(&buffer), [width, u64::MIN]); // `1` at element 1; `3` at element 0
    }

    /// An empty [`Vec`] locates no extreme item and is described as
    /// [`Basic`](manifest::Buffer::Basic).
    #[test]
    fn vec_empty_locates_nothing() {
        let data: Vec<u32> = Vec::new();
        let sector = Sector::new(8u64, 8u64).expect("Sector::new failed");
        let count = NonZeroU64::MIN;
        let buffer = data.describe(sector, count).expect("Describe failed");
        assert!(matches!(buffer, manifest::Buffer::Basic { .. }));
    }

    /// Statistics follow standard [`PartialOrd`] semantics: no ordering is invented for `NaN`,
    /// which compares `false` against every item and therefore never displaces a real extreme.
    #[test]
    fn vec_partial_ord_keeps_real_extremes_past_nan() {
        let data = vec![1.0, f64::NAN, 3.0, 2.0];
        let width = size_of::<f64>() as u64;
        let buffer = describe(&data, 4);
        assert_eq!(stats(&buffer), [u64::MIN, 2 * width]); // `1.0` at 0; `3.0` at element 2
    }

    /// A leading `NaN` opens the scan and is never displaced, so it becomes both extremes. The
    /// outcome is **conservative**: a `NaN` statistic satisfies no bounded predicate, so
    /// [`disjoint`](manifest::Buffer::disjoint) proves nothing and the buffer is retained.
    #[test]
    fn vec_leading_nan_is_conservative() {
        let data = vec![f64::NAN, 1.0, 3.0];
        let buffer = describe(&data, 3);
        assert_eq!(stats(&buffer), [u64::MIN, u64::MIN]); // the NaN at element 0
        let bytes = data.serialize().expect("Serialize failed");
        let mut mmap = MmapMut::map_anon(bytes.len()).expect("Anonymous map failed");
        mmap[..bytes.len()].copy_from_slice(&bytes);
        let mmap = mmap.make_read_only().expect("Read-only conversion failed");
        let manifest::Buffer::Detailed { min, max, .. } = &buffer else {
            panic!("Detailed")
        };
        // SAFETY: the statistic sectors span serialized `f64` items matching the requested type
        let min: f64 = min.slice(&mmap).expect("min").deserialize_into().expect("min decode");
        let max: f64 = max.slice(&mmap).expect("max").deserialize_into().expect("max decode");
        let keep = manifest::Buffer::disjoint(&min, &max, &(10.0f64..20.0)).not();
        assert!(keep); // NaN proves nothing; the buffer is retained rather than pruned
    }

    /// A float column always records statistics; `NaN` is an ordinary item under [`PartialOrd`] and
    /// never forces the [`Basic`](manifest::Buffer::Basic) fallback.
    #[test]
    fn vec_nan_still_detailed() {
        let data = vec![f64::NAN, f64::NAN];
        let buffer = describe(&data, 2);
        assert!(matches!(buffer, manifest::Buffer::Detailed { .. }));
    }

    /// [`Accumulate::describe`] resolves each located statistic onto its **absolute** position by
    /// shifting the relative sector past the buffer offset.
    #[test]
    fn describe_resolves_absolute_sectors() {
        let data: Vec<u32> = vec![3, 1, 2];
        let sector = Sector::new(64u64, 12u64).expect("Sector::new failed");
        let count = NonZeroU64::new(3).expect("Count is zero");
        let buffer = data.describe(sector, count).expect("Describe failed");
        let width = size_of::<u32>() as u64;
        assert_eq!(stats(&buffer), [64 + width, 64]); // `1` at element 1; `3` at element 0
        let manifest::Buffer::Detailed { min, .. } = buffer else {
            panic!("Buffer descriptor is not Detailed")
        };
        assert_eq!(min.size.get(), width); // spans exactly one serialized item
    }

    /// A niche body inlines [`Some`] and [`None`] directly, so the scan skips the absent items: a
    /// [`None`] slot carries no operand and its niche bytes are not a valid inner item.
    #[test]
    fn niche_skips_none() {
        let mut acc = OptInSitu::<NonZeroU64>::default();
        [NonZeroU64::new(7), None, NonZeroU64::new(3)].into_iter().for_each(|v| acc.push(v));
        let width = size_of_opt::<NonZeroU64>() as u64;
        let buffer = describe(&acc, 3);
        assert_eq!(stats(&buffer), [2 * width, u64::MIN]); // `3` at element 2, not the None
    }

    /// Only a byte-addressable accumulator implements [`MinMax`], so the type system alone
    /// prevents a [`Detailed`](manifest::Buffer::Detailed) descriptor for a column whose statistics
    /// could never be resolved: a bit-packed `bool` or an [unsized][1] row.
    ///
    /// [1]: https://doc.rust-lang.org/reference/dynamically-sized-types.html
    #[test]
    fn only_addressable_accumulators_are_extreme() {
        assert_impl_all!(Vec<u32>: MinMax);
        assert_impl_all!(OptInSitu<NonZeroU64>: MinMax);
        assert_impl_all!(OptBitVec<f64>: MinMax);
        assert_not_impl_any!(BitVec: MinMax); // one item per bit
        assert_not_impl_any!(OptBitVec<bool>: MinMax); // bit-packed data payload
        assert_not_impl_any!(Seq<u8>: MinMax); // unsized rows
        assert_not_impl_any!(OptSeq<u8>: MinMax);
    }

    /// An [`OptBitVec`] descriptor reports the **whole** buffer body and the **logical** item
    /// count, while its statistics point at exactly one item **inside** that body. The `data`
    /// sub-buffer holds the [`Some`] items alone, so the descriptor frame and the statistics frame
    /// deliberately diverge.
    #[test]
    fn opt_bit_vec_statistics_span_one_item_within_body() {
        let mut acc = OptBitVec::<u32>::default();
        [Some(3u32), None, Some(1), Some(2)].into_iter().for_each(|v| acc.push(v));
        let size = acc.size().expect("Size failed");
        let body = Sector::new(64u64, size).expect("Sector::new failed");
        let count = NonZeroU64::new(4).expect("Count is zero");
        let out = acc.describe(body, count).expect("Describe failed");
        let manifest::Buffer::Detailed { buffer, count, min, max, .. } = out else {
            panic!("Buffer descriptor is not Detailed")
        };
        assert_eq!(buffer, body); // whole body, spanning every item
        assert_eq!(count.get(), 4); // logical count, including the absent item
        let width = size_of::<u32>() as u64;
        assert_eq!(min.size.get(), width); // exactly one item
        assert_eq!(max.size.get(), width);
        let end = body.next().expect("Sector overflow").get();
        assert!(min.offset >= body.offset && min.offset + min.size.get() <= end);
        assert!(max.offset >= body.offset && max.offset + max.size.get() <= end);
        // Each statistic resolves to the correct Some item within the data sub-buffer
        let origin = body.offset + acc.origin().expect("Origin failed");
        assert_eq!(min.offset, origin + width); // `1` is Some-element 1
        assert_eq!(max.offset, origin); // `3` is Some-element 0
    }

    /// An all-[`None`] niche column locates no extreme item and is described as
    /// [`Basic`](manifest::Buffer::Basic).
    #[test]
    fn niche_all_none_locates_nothing() {
        let mut acc = OptInSitu::<NonZeroU64>::default();
        [None, None].into_iter().for_each(|v| acc.push(v));
        assert!(matches!(describe(&acc, 2), manifest::Buffer::Basic { .. }));
    }

    /// [`Accumulate::discard`] empties the buffer and resets [`Accumulate::count`] to zero.
    #[test]
    fn accumulate_discard_resets_count() {
        let mut data = vec![1, 2, 3];
        assert_eq!(Accumulate::count(&data), 3);
        data.discard();
        assert!(Accumulate::is_empty(&data));
        assert_eq!(Accumulate::count(&data), 0);
    }

    /// A materialised `bool` column is [described](Accumulate::describe) as
    /// [`Basic`](manifest::Buffer::Basic) losslessly: [`BitVec`] packs one item per bit, which no
    /// byte [`Sector`] can span, and a `bool` buffer holding both items always spans `false..=true`
    /// so statistics could never prune it.
    #[test]
    fn bit_vec_describes_basic() {
        let data: BitVec = [true, false, true].into_iter().collect();
        assert!(matches!(describe(&data, 3), manifest::Buffer::Basic { .. }));
    }

    /// A `bool` column holds only `true` and `false`, so the descriptor follows from uniformity
    /// alone: a uniform run stays [`Lite`](Buffer::Compact) and is described as
    /// [`Compact`](manifest::Buffer::Compact), while a mixed column materialises and is described
    /// as [`Basic`](manifest::Buffer::Basic).
    #[test]
    fn bool_column_descriptors() {
        let describe = |items: &[bool]| {
            let mut acc: Buffer<bool> = Buffer::default();
            items.iter().for_each(|&v| acc.push(v));
            let mut col = Column::from(bool::with_unfolder::<Schema>());
            acc.buffers(0, &mut std::iter::once(&mut col)).expect("Buffers failed");
            col.buffers.into_iter().next().expect("Buffer absent")
        };
        let uniform = describe(&[true, true, true]);
        let absent = describe(&[false, false]);
        let mixed = describe(&[true, false, true]);
        assert!(matches!(uniform, manifest::Buffer::Compact { .. }));
        assert!(matches!(absent, manifest::Buffer::Compact { .. }));
        assert!(matches!(mixed, manifest::Buffer::Basic { .. }));
    }

    /// [`Align::align`] rounds [`size`](Serialize::size) up ↑ to the next 64-bit boundary.
    #[test]
    fn aligned_rounds_size() {
        let data: Vec<u16> = vec![1, 2, 3]; // 6 byte payload; excludes the length prefix
        let size = data.size().expect("Size failed");
        assert_eq!(size.get(), 6);
        assert_eq!(size.align().expect("Align failed"), 8);
    }

    /// [`SizedBuf`] frames the payload behind its length prefix and adds zero-bytes up to the
    /// next 64-bit boundary; an empty payload is a genuine error rather than a zero prefix.
    #[test]
    fn sized_buf_frames_payload() {
        let data: Vec<u16> = vec![1, 2, 3];
        let framed = SizedBuf::new(&data);
        assert_eq!(framed.size().expect("Size failed").get(), 16); // 8 prefix + 6 payload → 8
        let bytes = framed.serialize().expect("Serialize failed");
        assert_eq!(bytes[..8], 6u64.to_le_bytes()); // Length prefix excludes padding
        assert_eq!(bytes[14..], [u8::MIN; 2]); // Trailing bytes are zero-filled
        let none: Vec<u16> = Vec::new();
        SizedBuf::new(&none).size().expect_err("Empty region framed"); // omitted, never framed
    }

    /// [`OptBitVec`] aligns the value buffer to the boundary following the validity mask and stores
    /// only [`Some`] items in the concatenated payload.
    #[test]
    fn opt_bit_vec_layout() {
        let mut acc: OptBitVec<u32> = OptBitVec::default();
        [Some(1u32), None, Some(3)].into_iter().for_each(|v| acc.push(v));
        let bytes = acc.serialize().expect("Serialize failed");
        assert_eq!(bytes.len(), 32); // [mask 9 → 16][data 16]
        assert_eq!(acc.size().expect("Size failed").get(), 32); // Body excludes the outer prefix
        assert_eq!(SizedBuf::new(&acc).size().expect("Size failed").get(), 40); // Framed body
        assert_eq!(bytes[..8], 1u64.to_le_bytes()); // Mask length prefix records exact size
        assert_eq!(bytes[8], 0b101); // Mask bits in Lsb0 order
        assert_eq!(bytes[9..16], [u8::MIN; 7]); // Mask padding bytes are zero-filled
        assert_eq!(bytes[16..24], 8u64.to_le_bytes()); // item length prefix excludes None rows
        assert_eq!(bytes[24..28], 1u32.to_le_bytes()); // Only Some items are stored, contiguously
        assert_eq!(bytes[28..32], 3u32.to_le_bytes());
    }

    /// An all-[`None`] [`OptBitVec`] omits the empty item sub-buffer entirely: the body carries
    /// only the framed validity mask and zero-length regions never reach the disk.
    #[test]
    fn opt_bit_vec_layout_all_none() {
        let mut acc: OptBitVec<u32> = OptBitVec::default();
        [None, None, None::<u32>].into_iter().for_each(|v| acc.push(v));
        let bytes = acc.serialize().expect("Serialize failed");
        assert_eq!(bytes.len(), 16); // [mask 9 → 16]; the empty data sub-buffer is omitted
        assert_eq!(bytes[..8], 1u64.to_le_bytes()); // Mask length prefix records exact size
        assert_eq!(bytes[8], 0b000); // Every row is None
        assert_eq!(bytes[9..], [u8::MIN; 7]); // Mask padding bytes are zero-filled
    }

    /// [`Seq`] offsets terminate on the boundary; data follows without intermediate padding.
    #[test]
    fn seq_layout() {
        let mut acc: Seq<u8> = Seq::default();
        acc.push(vec![97, 98, 99]);
        acc.push(vec![100, 101]);
        let bytes = acc.serialize().expect("Serialize failed");
        assert_eq!(bytes.len(), 40); // [offsets 24][data 13 → 16]
        assert_eq!(SizedBuf::new(&acc).size().expect("Size failed").get(), 48); // Framed body
        assert_eq!(bytes[..8], 16u64.to_le_bytes()); // Offsets length prefix records exact size
        assert_eq!(bytes[8..16], 3u64.to_le_bytes()); // Zero-based cumulative end of row 0
        assert_eq!(bytes[16..24], 5u64.to_le_bytes()); // Zero-based cumulative end of row 1
        assert_eq!(bytes[24..32], 5u64.to_le_bytes()); // Data length prefix records exact size
        assert_eq!(bytes[32..37], [97, 98, 99, 100, 101]);
        assert_eq!(bytes[37..40], [u8::MIN; 3]); // Data padding bytes are zero-filled
    }

    /// [`Describe::buffers`] records exact sector lengths and returns aligned offsets.
    #[test]
    fn buffers_align_offsets() {
        let mut data: Buffer<u16> = Buffer::default();
        [1u16, 2, 3].into_iter().for_each(|v| data.push(v)); // Distinct items ⇒ Full
        let mut col = Column::from(u16::with_unfolder::<Schema>());
        let next = data.buffers(0, &mut std::iter::once(&mut col)).expect("Buffers failed");
        assert_eq!(next, 16); // Next buffer begins at 64-bit alignment boundary
        let manifest::Buffer::Detailed { buffer, .. } = col.buffers.first().expect("Buffer absent")
        else {
            panic!("Buffer descriptor is not Detailed")
        };
        assert_eq!(buffer.offset, 8); // Body starts after the header prefix
        assert_eq!(buffer.size.get(), 6); // Body excludes the prefix and padding
    }

    /// [`OptBitVec`] records the data buffer at its aligned offset inside the composite region, and
    /// lifts the statistics located across its [`Some`] subset onto the whole buffer.
    #[test]
    fn opt_bit_vec_buffers_offset() {
        let mut acc: Buffer<Option<u32>> = Buffer::default();
        [Some(1u32), None, Some(3)].into_iter().for_each(|v| acc.push(v)); // Distinct ⇒ Full
        let mut col = Column::from(u32::with_unfolder::<Schema>());
        let next = acc.buffers(0, &mut std::iter::once(&mut col)).expect("Buffers failed");
        assert_eq!(next, 40); // Aligned end of the composite region
        let manifest::Buffer::Detailed { buffer, count, min, max, .. } =
            col.buffers.first().expect("Buffer absent")
        else {
            panic!("Buffer descriptor is not Detailed")
        };
        assert_eq!(buffer.offset, 8); // Whole body starts after the header prefix
        assert_eq!(buffer.size.get(), 32); // Body spans the mask and data regions
        // The descriptor records the LOGICAL item count, not the two Some items that carry data;
        // recording the data sub-buffer count here would truncate the column at read time.
        assert_eq!(count.get(), 3);
        // The data sub-buffer holds the Some items alone, packed after the framed mask region.
        let data = buffer.offset + 16 + SizedBuf::<Vec<u32>>::PREFIX;
        assert_eq!(min.offset, data); // Some(1) at data slot 0
        assert_eq!(max.offset, data + size_of::<u32>() as u64); // Some(3) at data slot 1
        assert_eq!(min.size.get(), size_of::<u32>() as u64);
    }

    /// [`Seq<u8>`] accumulates a [`String`] into the identical layout as its raw UTF-8 bytes.
    #[test]
    fn seq_string_matches_bytes() {
        let mut text: Seq<u8> = Seq::default();
        text.push(String::from("héllo"));
        text.push(String::from("xyz"));
        let mut bytes: Seq<u8> = Seq::default();
        bytes.push("héllo".as_bytes().to_vec());
        bytes.push(b"xyz".to_vec());
        let text = text.serialize().expect("Serialize failed");
        let bytes = bytes.serialize().expect("Serialize failed");
        assert_eq!(text, bytes);
    }

    /// [`OptSeq<u8>`] accumulates an [`Option<String>`] identically to its raw optional UTF-8
    /// bytes; the [`u64::MAX`] sentinel marks [`None`] in both.
    #[test]
    fn opt_seq_string_matches_bytes() {
        let mut text: OptSeq<u8> = OptSeq::default();
        text.push(Some(String::from("ab")));
        text.push(None::<String>);
        text.push(Some(String::from("c")));
        let mut bytes: OptSeq<u8> = OptSeq::default();
        bytes.push(Some(b"ab".to_vec()));
        bytes.push(None::<Vec<u8>>);
        bytes.push(Some(b"c".to_vec()));
        let text = text.serialize().expect("Serialize failed");
        let bytes = bytes.serialize().expect("Serialize failed");
        assert_eq!(text, bytes);
    }

    /// [`Buffer`] counts repetitions of one item in place without materialising the inner
    /// accumulator.
    #[test]
    fn compact_counts_repetitions() {
        let mut acc: Buffer<u32> = Buffer::default();
        assert!(acc.is_empty());
        [5, 5, 5].into_iter().for_each(|v| acc.push(v));
        assert!(matches!(acc, Buffer::Compact { count: 3, .. }));
        assert!(!acc.is_empty());
        assert_eq!(acc.count(), 3);
    }

    /// A [`Lite`](Buffer::Compact) column serializes as a **one-item** compact body regardless of
    /// the repetition count.
    #[test]
    fn compact_lite_serializes_one_row() {
        let mut acc: Buffer<u32> = Buffer::default();
        [5, 5, 5].into_iter().for_each(|v| acc.push(v));
        let one = vec![5u32].serialize().expect("Serialize failed");
        assert_eq!(acc.size().expect("Size failed").get(), 4);
        assert_eq!(acc.serialize().expect("Serialize failed"), one);
    }

    /// The first differing push collects the repeated run into a materialised
    /// [`Full`](Buffer::Many) state that is byte-identical to a hand-built inner accumulator.
    #[test]
    fn compact_materialises_full() {
        let mut acc: Buffer<u32> = Buffer::default();
        [5, 5, 5, 7].into_iter().for_each(|v| acc.push(v));
        assert!(matches!(acc, Buffer::Many(..)));
        assert_eq!(acc.count(), 4);
        let full = vec![5u32, 5, 5, 7].serialize().expect("Serialize failed");
        assert_eq!(acc.serialize().expect("Serialize failed"), full);
    }

    /// An all-[`None`] optional column stays [`Lite`](Buffer::Compact) and serializes as a one-row
    /// mask-only body; the empty data sub-buffer is omitted entirely.
    #[test]
    fn compact_all_none_lite_body() {
        let mut acc: Buffer<Option<u32>> = Buffer::default();
        [None, None, None::<u32>].into_iter().for_each(|v| acc.push(v));
        assert!(matches!(acc, Buffer::Compact { count: 3, .. }));
        let bytes = acc.serialize().expect("Serialize failed");
        assert_eq!(bytes.len(), 16); // [mask 9 → 16]; one None row, data omitted
        assert_eq!(bytes[..8], 1u64.to_le_bytes()); // Mask length prefix records exact size
        assert_eq!(bytes[8], 0b0); // The single row is None
    }

    /// [`BitMatch::eq`] compares the exact bit pattern: a repeated [`f64::NAN`] niche column stays
    /// [`Lite`](Buffer::Compact), while a differing bit pattern materialises
    /// [`Full`](Buffer::Many).
    #[test]
    fn compact_float_bits_drive_state() {
        let mut nan: Buffer<f64> = Buffer::default();
        [f64::NAN, f64::NAN].into_iter().for_each(|v| nan.push(v));
        assert!(matches!(nan, Buffer::Compact { count: 2, .. }));
        let mut inf: Buffer<f64> = Buffer::default();
        [f64::INFINITY, f64::INFINITY].into_iter().for_each(|v| inf.push(v));
        assert!(matches!(inf, Buffer::Compact { count: 2, .. }));
        inf.push(f64::NEG_INFINITY);
        assert!(matches!(inf, Buffer::Many(..)));
    }

    /// [`Accumulate::discard`] returns the column to the [`Empty`](Buffer::Empty) state.
    #[test]
    fn compact_discard_resets_empty() {
        let mut acc: Buffer<u32> = Buffer::default();
        [5, 7].into_iter().for_each(|v| acc.push(v));
        assert!(matches!(acc, Buffer::Many(..)));
        acc.discard();
        assert!(matches!(acc, Buffer::Empty));
        assert!(acc.is_empty());
    }

    /// [`Describe::buffers`] registers a [`manifest::Buffer::Compact`](manifest::Buffer::Compact)
    /// descriptor whose sector spans the one-item body; materialising emits
    /// [`manifest::Buffer::Detailed`](manifest::Buffer::Detailed) instead.
    #[test]
    fn compact_buffers_emit_compact() {
        let mut acc: Buffer<u16> = Buffer::default();
        [7, 7, 7].into_iter().for_each(|v| acc.push(v));
        let mut col = Column::from(u16::with_unfolder::<Schema>());
        let next = acc.buffers(0, &mut std::iter::once(&mut col)).expect("Buffers failed");
        assert_eq!(next, 16); // Aligned end of the one-item compact body
        let manifest::Buffer::Compact { buffer, count, .. } =
            col.buffers.first().expect("Buffer absent")
        else {
            panic!("Buffer descriptor is not Compact")
        };
        assert_eq!(buffer.offset, 8); // Body starts after the header prefix
        assert_eq!(buffer.size.get(), 2); // Body spans exactly one serialized u16
        assert_eq!(count.get(), 3); // Repetition count spans every accumulated item
        acc.push(9); // Materialise the inner accumulator
        let mut col = Column::from(u16::with_unfolder::<Schema>());
        acc.buffers(0, &mut std::iter::once(&mut col)).expect("Buffers failed");
        assert!(matches!(
            col.buffers.first().expect("Buffer absent"),
            manifest::Buffer::Detailed { .. }
        ));
    }

    /// [`Accumulate::describe`] constructs a
    /// [`manifest::Buffer::Compact`](manifest::Buffer::Compact) descriptor for a homogeneous run:
    /// three identical pushes record `count == 3` spanning the one serialized item, and no
    /// statistic is recorded because the sector already spans it.
    #[test]
    fn compact_describes_compact() {
        let mut acc: Buffer<u16> = Buffer::default();
        [4, 4, 4].into_iter().for_each(|v| acc.push(v));
        let manifest::Buffer::Compact { count, buffer, .. } = describe(&acc, acc.count()) else {
            panic!("Buffer descriptor is not Compact")
        };
        assert_eq!(count.get(), 3); // Repetition count spans every accumulated item
        assert_eq!(buffer.size.get(), 2); // Body spans exactly one serialized u16
    }

    /// A materialised [`Full`](Buffer::Many) run records its statistics as **sectors** pointing at
    /// the extreme items inside its own body; each spans exactly one serialized item.
    #[test]
    fn compact_full_buffer_stats() {
        let mut acc: Buffer<u32> = Buffer::default();
        [1u32, 2, 3].into_iter().for_each(|v| acc.push(v)); // Distinct ⇒ Full
        let width = size_of::<u32>() as u64;
        let buffer = describe(&acc, acc.count());
        assert_eq!(stats(&buffer), [u64::MIN, 2 * width]); // the `1` at +0; the `3` at +8
        let manifest::Buffer::Detailed { min, max, .. } = buffer else {
            panic!("Buffer descriptor is not Detailed")
        };
        assert_eq!(min.size.get(), width); // each statistic spans exactly one serialized item
        assert_eq!(max.size.get(), width);
    }

    /// A [`String`] column is not byte-addressable, so a materialised run emits
    /// [`Basic`](manifest::Buffer::Basic) and is never pruned.
    #[test]
    fn compact_buffers_emit_basic() {
        let mut acc: Buffer<String> = Buffer::default();
        ["red", "blue"].into_iter().for_each(|v| acc.push(String::from(v)));
        let mut col = Column::from(String::with_unfolder::<Schema>());
        acc.buffers(0, &mut std::iter::once(&mut col)).expect("Buffers failed");
        assert!(matches!(
            col.buffers.first().expect("Buffer absent"),
            manifest::Buffer::Basic { .. }
        ));
    }

    /// An [`Empty`](Buffer::Empty) [`Buffer`] surfaces [`Error::Zero`] from
    /// [`describe`](Accumulate::describe); empty buffers are never written to disk and must be
    /// caught before registration. Scope is reserved here for future default-item buffer omission.
    #[test]
    fn compact_empty_buffer_errors() {
        let acc: Buffer<u32> = Buffer::Empty;
        let sector = Sector {
            offset: SizedBuf::<u8>::PREFIX,
            size: NonZeroU64::MIN,
        };
        let count = NonZeroU64::MIN; // an Empty accumulator never reaches a non-zero count
        assert!(matches!(acc.describe(sector, count), Err(Error::Zero)));
    }

    /// [`Serialize`] for byte [slices][1] copies verbatim; empty slices are rejected because every
    /// on-disk region records a [`NonZeroU64`] size.
    ///
    /// [1]: https://doc.rust-lang.org/std/primitive.slice.html
    #[test]
    fn slice_serialize_verbatim() {
        let data: &[u8] = &[1, 2, 3];
        assert_eq!(data.size().expect("Size failed").get(), 3);
        assert_eq!(data.serialize().expect("Serialize failed"), vec![1, 2, 3]);
        let none: &[u8] = &[];
        none.size().expect_err("Empty accumulator sized");
    }
}
