/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! A composable [`Query`] interface to [read](Read) data from any [msca](crate) file.
//!
//! ---
//!
//! Each new `Query` begins with every [`Column`](manifest::Column) and every [`Buffer`] from the
//! specified [`Schema`].
//!
//! - Use [`Query::read`] or [`Query::iter`] to pull every item from every column without filters.
//! - Use [`Query::column`] to extract individual columns which can then be [filtered](Filter).
//!
//! Filters subtractively reduce the result set. Filters can act at two points in the query
//! lifecycle: **buffer filters** are evaluated **before** file [`IO`](io); **item filters** are
//! evaluated **after** [deserialization](Deserialize). Every item is deserialized exactly once and
//! every infallible filter [`Fn`] is monomorphized by the compiler.
//!
//! ```rust,ignore
//! let overheating = dataset
//!     .query("schema_name")?
//!     .column::<f64>("temperature")?
//!     .range(35.0..)?
//!     .iter();
//! ```
//!
//! Items are deserialized lazily when the [`Iterator`] returned by a terminal method is polled.

#![doc = include_str!("../../../doc/query-filters.md")]
#![doc = include_str!("../../../doc/query-columns.md")]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::{self, Display};
use std::hash::Hash;
use std::marker::PhantomData;
use std::num::TryFromIntError;
use std::ops::{Deref, RangeBounds};

use bitvec::boxed::BitBox;
use bitvec::vec::BitVec;
use funty::Unsigned;
use memmap2::Mmap;
use xxhash_rust::xxh3::Xxh3Builder;

use crate::io::{self, Deserialize, Deserializer};
use crate::manifest::{self, Buffer};
use crate::read::{Composite, Evaluate, IsOption, Outcome, Read, Reader, Squash, Unfiltered};
use crate::schema::{self, Schema, Type, Unfolder, number};

/* ------------------------------------------------------------------------------ Public Exports */

/// A composable query interface to [read](Read) data from any [msca](crate) file; initialised from
/// [`Dataset::query`][1] and executed lazily when [`iter`](Self::iter) is polled.
///
/// [`Query`] also provides a [column](Adapter) factory for the specified [`Schema`]. The query
/// lifetime `'d` is tied to the underlying [`Dataset`](crate::Dataset).
///
/// Refer to the [module-level documentation](self) for implementation details.
///
/// [1]: crate::Dataset::query
#[derive(Clone, Copy, Debug)]
pub struct Query<'d> {
    /// Read-only [memory map](Mmap) backed by the immutable segment region.
    ///
    /// Refer to the [safety documentation](io::File::mmap) for details.
    pub(crate) mmap: &'d Mmap,
    /// On-disk [`Schema`][1] borrowed from the [manifest] with [columns][2] keyed by name.
    ///
    /// [1]: manifest::Schema
    /// [2]: manifest::Column
    pub schema: &'d manifest::Schema,
}

impl<'d> Query<'d> {
    /// Map each **distinct** item to the corresponding on-disk index.
    ///
    /// The [`Dataset`][1] is read in ascending insertion order; items record their first index and
    /// subsequent duplicate items are discarded.
    ///
    /// ### Errors
    ///
    /// - [`Error::Number`] if a first-occurrence index overflows `N`.
    /// - [`Error::Io`] if a deserialization failure occurs.
    ///
    /// [1]: crate::dataset::Dataset
    pub fn into_hash_map<I, N>(self) -> Result<HashMap<I, N, Xxh3Builder>, Error>
    where
        N: Unsigned,
        I: Unfiltered<'d> + Eq + Hash + 'd,
    {
        let iter = self.iter::<I>()?;
        Self::intern(iter).map_err(Error::from)
    }

    /// Intern each **distinct** item from the provided set and map to the corresponding index of
    /// the earliest on-disk occurrence.
    ///
    /// The index increments for each on-disk item. Repeated items intern to the index of their
    /// earlier occurrence while the counter advances `+1` for each duplicate. The maximum index is
    /// therefore greater than or equal to `≥` the number of [`HashMap`] entries.
    ///
    /// ### Errors
    ///
    /// - [`Error::Number`][1] if an index overflows `N`.
    /// - [`Error::Io`][2] if a deserialization failure occurs.
    ///
    /// Refer to [`Query::into_hash_map`] and [`Adapter::into_hash_map`] for the entry points.
    ///
    /// [1]: io::Error::Number
    /// [2]: io::Error::Io
    fn intern<I, N, S>(items: S) -> Result<HashMap<I, N, Xxh3Builder>, io::Error>
    where
        N: Unsigned,
        I: Eq + Hash,
        S: Iterator<Item = Result<I, io::Error>>,
    {
        let mut map = HashMap::with_hasher(Xxh3Builder::new());
        let mut next = Some(N::MIN);
        for item in items {
            let i = next.ok_or(number::Error::Zero)?;
            map.entry(item?).or_insert(i);
            next = i.checked_add(N::ONE);
        }
        Ok(map)
    }

    /// Select a named [`Column`](manifest::Column) from the parent [`Query`].
    ///
    /// The requested type is verified against the actual on-disk column [`Type`] exactly once.
    /// Subsequent column operations – such as filtering and deserialization – can progress
    /// fearlessly without further runtime checks.
    ///
    /// ```rust,ignore
    /// .column::<f64>("temperature")? // a typed handle over the "temperature" column
    /// ```
    ///
    /// ### Errors
    ///
    /// - [`Error::Column`] if `name` is not found in the [`Schema`](manifest::Schema).
    /// - [`Error::Type`] if the requested `Type` does not match the on-disk column type.
    pub fn column<I>(&self, name: &str) -> Result<Column<'d, I>, Error>
    where
        I: Read<'d> + Clone + 'd,
        Schema: Unfolder<I>,
    {
        if let Some(entry) = self.schema.columns.get(name) {
            let buffers = &entry.exact::<I>()?.buffers;
            // SAFETY: on-disk column type verified against requested I via manifest::Column::exact
            let column = Src { query: *self, buffers }.into();
            Ok(column)
        } else {
            Error::Column { name: name.into() }.into()
        }
    }

    /// Returns the `n`th item of the query.
    ///
    /// Like most indexing operations, the count starts from zero, so `nth(0)` returns the first
    /// item, `nth(1)` the second, and so on.
    ///
    /// Returns [`None`] if `n` exceeds the number of on-disk items written for the [`Schema`].
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Io`] if an error occurs during file [`IO`](io) or item deserialization.
    pub fn nth<I>(self, n: usize) -> Result<Option<I>, Error>
    where
        I: Unfiltered<'d> + 'd,
    {
        I::nth(self, n)?.included().next().transpose().map_err(Error::from)
    }

    /// Return an [`Iterator`] yielding one [`Outcome`] per [deserialized][1] item from the named
    /// [`Column`](manifest::Column).
    ///
    /// The requested [`Type`] is verified against the on-disk column type exactly once. Subsequent
    /// deserialization proceeds fearlessly without additional runtime checks.
    ///
    /// ### Guidance
    ///
    /// Use zero-allocation `Query::read` when no filter is required for the named column. Use
    /// [`Query::column`] to extract the named column when filters *are* required. An unfiltered
    /// extracted column retains every [`Buffer`], meaning both unfiltered forms yield the same
    /// items in the same order.
    ///
    /// ### Errors
    ///
    /// - [`Error::Column`] if `name` is not found in the [`Schema`](manifest::Schema).
    /// - [`Error::Type`] if the requested type is incompatible with the on-disk column type.
    ///
    /// Refer to [`Query::iter`] for a resolved alternative that automatically re-polls the iterator
    /// to yield only [included](Outcome::Include) items.
    ///
    /// [1]: Deserialize::deserialize
    pub fn read<I>(self, name: &str) -> Result<impl Iterator<Item = Outcome<I>> + 'd, Error>
    where
        I: Read<'d> + Clone + 'd,
        Schema: Unfolder<I>,
    {
        let buffers = self
            .schema
            .columns
            .get(name)
            .ok_or_else(|| Error::Column { name: name.into() })?
            .exact::<I>()?
            .buffers
            .iter();
        let items = iter::Src::new(buffers, self.mmap).into_iter().map(Outcome::from);
        Ok(items)
    }

    /// Returns an [`Iterator`] that yields [`Composite`] items.
    ///
    /// ### Implementation
    ///
    /// Each field of the composite is lazily [deserialized][1] from the respective [`Column`][2].
    /// Refer to the [unfiltered trait documentation](Unfiltered) for more details.
    ///
    /// ### Guidance
    ///
    /// The iterator automatically re-polls the [`Source`] until an [included](Outcome::Include)
    /// item is returned. Use [`Query::read`] for a non-resolved alternative that yields [`Outcome`]
    /// instead.
    ///
    /// ### Errors
    ///
    /// - [`Error::Column`] if a column named by the composite `I` is absent from the schema.
    /// - [`Error::Type`] if the requested [`Type`] does not match the on-disk column type.
    ///
    /// Refer to [`Query::read`] for a non-resolved alternative that yields [`Outcome`].
    ///
    /// [1]: Deserialize::deserialize
    /// [2]: manifest::Column
    pub fn iter<I>(self) -> Result<impl Iterator<Item = Result<I, io::Error>> + 'd, Error>
    where
        I: Unfiltered<'d> + 'd,
    {
        let iter = I::unfiltered(self)?.included();
        Ok(iter)
    }

    /// Returns the total number of on-disk items for this [`Schema`] across every segment; the sum
    /// of [`Buffer`](Buffer)`::`[`count`](Buffer::count) for one [`Column`](manifest::Column).
    pub fn count(self) -> u64 {
        self.schema.count()
    }
}

impl<'d> PartialEq for Query<'d> {
    /// Returns `true` if two queries read the same [`Schema`](manifest::Schema).
    ///
    /// Read the [trait documentation](PartialEq) for more details.
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.schema, other.schema)
    }
}

impl<'d> Eq for Query<'d> {}

/// An immutable **byte source** for one [`Column`] and all subsequent [adapters](Adapter).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Src<'d> {
    /// An immutable reference to the parent [`Query`].
    query: Query<'d>,
    /// [`Buffer`] descriptors for the [`Column`][1] across all segments in [`Sector`][2] order.
    ///
    /// [1]: manifest::Column
    /// [2]: io::Sector
    // NOTE: sector offset increases monotonically → sector order matches on-disk segment order
    buffers: &'d BTreeSet<Buffer>,
}

impl<'d> Src<'d> {
    /// Returns a new [mask](Exclude) that includes every [`Buffer`] for this column.
    ///
    /// `Buffer` inclusion is determined using a positional mask where the `n`th bit corresponds
    /// to the `n`th buffer from the `n`th data segment.
    ///
    /// ```text
    /// buffers   [ A ][ B ][ C ][ D ][ E ]    Immutable borrowed buffer set.
    /// mask        1    0    1    1    0      Mutable owned bitmask.
    ///             ▼         ▼    ▼
    /// read        A         C    D           Buffers B and E are never read.
    /// ```
    ///
    /// [Filters](Filter) are applied subtractively to reduce the mask.
    pub fn mask(&self) -> BitBox {
        let n = self.buffers.len();
        BitVec::repeat(true, n).into_boxed_bitslice()
    }

    /// Returns the logical number of items recorded in each [`Buffer`].
    ///
    /// ### Errors
    ///
    /// Returns [`io::Error::Number`] if any recorded count overflows [`usize`].
    pub(crate) fn counts(&self, mask: &BitBox) -> impl Iterator<Item = Result<usize, io::Error>> {
        let bits = mask.iter().by_vals();
        self.buffers.iter().zip(bits).map(|e| match e.1 {
            true => e.0.count().try_into().map_err(io::Error::from),
            false => Ok(usize::MIN),
        })
    }

    /// Returns **only** the [`Buffer`] descriptors included by the [mask](Exclude) in [`Sector`][1]
    /// order.
    ///
    /// [1]: io::Sector
    // NOTE: owned slice can outlive the mask; Box prevents resize allocations at the type level
    pub(crate) fn retained(&self, mask: &BitBox) -> Box<[&'d Buffer]> {
        mask.iter().by_vals().zip(self.buffers).filter(|b| b.0).map(|b| b.1).collect()
    }

    /// An iterator method that applies a fallible [test](FnMut) to each [`Buffer`] descriptor:
    ///
    /// - Skips any buffers that are already [excluded](Exclude) by the [mask](BitBox).
    /// - Retains all buffers for which `test` returns `true`.
    /// - Excludes any buffers for which `test` returns `false`.
    ///
    /// Returns the number of included buffers. Refer to [`Src::mask`] for more details.
    ///
    /// ### Errors
    ///
    /// Forwards [`io::Error`] from the fallible `test` function.
    pub(crate) fn try_exclude<F>(&self, mask: &mut BitBox, mut test: F) -> Result<usize, io::Error>
    where
        F: FnMut(&Buffer, &'d Mmap) -> Result<bool, io::Error>,
    {
        mask.iter_mut().zip(self.buffers).try_fold(usize::MIN, |n, (bit, buf)| {
            if *bit {
                match test(buf, self.query.mmap)? {
                    true => return Ok(n + 1),
                    false => bit.commit(false),
                }
            };
            Ok(n)
        })
    }
}

/// A **strongly typed data source** for one [`Column`] and all subsequent [adapters](Adapter).
///
/// This type fixes a generic [byte source](Src) to a specified [`item`](I) type that is
/// [verified](manifest::Column::exact) exactly once against the actual on-disk column type during
/// [initialisation](Query::column). This design enables the compiler to [monomorphize][1] all
/// subsequent operations and progress fearlessly without runtime type checks.
///
/// [1]: https://rustc-dev-guide.rust-lang.org/backend/monomorph.html
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Column<'d, I> {
    /// The on-disk source this column reads from.
    src: Src<'d>,
    /// Zero-sized type-state for the requested [`item`](I) type.
    item: PhantomData<I>,
}

impl<'d, I> From<Src<'d>> for Column<'d, I> {
    fn from(src: Src<'d>) -> Self {
        Self { src, item: PhantomData }
    }
}

impl<'d, I> Deref for Column<'d, I> {
    type Target = Src<'d>;

    fn deref(&self) -> &Src<'d> {
        &self.src
    }
}

/* ------------------------------------------------------------------------------ Column Filters */

/// A [column][1] [adapter](Adapter) that applies a [Filter] to each [deserialized](Deserialize)
/// item **without** [detailed](Buffer::Detailed) buffer exclusion using statistics.
///
/// ### Implementation
///
/// This adapter captures the filter operand `&I` into a [`Fn`] that is used to assess each
/// [compact](Buffer::Compact) buffer and [deserialized](Deserialize) item. This adapter cannot
/// use buffer statistics to exclude [detailed](Buffer::Detailed) candidates.
///
/// Use a named adapter e.g. [`Range`] for filters that **can** assess detailed candidates.
///
/// [1]: manifest::Column
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Filter<'d, S, F, I>
where
    S: Source<'d>,
    F: Fn(&I) -> bool,
{
    /// The wrapped data source which yields [deserialized](Deserialize) items for the
    /// [`filter`](filter::Filter::filter) closure.
    source: S,
    /// The [`filter`](filter::Filter::filter) used to assess each [deserialized](Deserialize) item.
    filter: F,
    /// Zero-sized **marker** carrying the item type and [`Dataset`][1] lifetime.
    ///
    /// [1]: crate::dataset::Dataset
    phantom: PhantomData<&'d I>,
}

impl<'d, S, F, I> Deref for Filter<'d, S, F, I>
where
    S: Source<'d>,
    F: Fn(&I) -> bool,
{
    type Target = Src<'d>;

    fn deref(&self) -> &Src<'d> {
        &self.source
    }
}

/// A [column][1] [adapter](Adapter) that retains **only** items within the specified [range][2].
///
/// [1]: manifest::Column
/// [2]: RangeBounds
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
// NOTE: this struct is used for both "mask" and "iter" adapter chains
pub struct Range<'d, S, B, I>
where
    S: Source<'d>,
    B: RangeBounds<I>,
{
    /// The wrapped data [`Source`] which yields [deserialized](Deserialize) items.
    source: S,
    /// The [range](RangeBounds) of items to retain.
    bounds: B,
    /// Zero-sized **marker** carrying the item type and [`Dataset`][1] lifetime.
    ///
    /// [1]: crate::dataset::Dataset
    phantom: PhantomData<&'d I>,
}

impl<'d, S, B, I> Deref for Range<'d, S, B, I>
where
    S: Source<'d>,
    B: RangeBounds<I>,
{
    type Target = Src<'d>;

    fn deref(&self) -> &Src<'d> {
        &self.source
    }
}

/// A [column][1] [adapter](Adapter) that retains **only** items [matching](BitMatch) one target.
///
/// [1]: manifest::Column
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
// NOTE: this struct is used for both "mask" and "iter" adapter chains
pub struct BitMatch<'d, S, I>
where
    S: Source<'d>,
    I: schema::BitMatch,
{
    /// The wrapped data [`Source`] which yields [deserialized](Deserialize) items.
    source: S,
    /// The target against which each [deserialized](Deserialize) item is [assessed](BitMatch).
    item: I,
    /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
    phantom: PhantomData<&'d Mmap>,
}

impl<'d, S, I> Deref for BitMatch<'d, S, I>
where
    S: Source<'d>,
    I: schema::BitMatch,
{
    type Target = Src<'d>;

    fn deref(&self) -> &Src<'d> {
        &self.source
    }
}

/// A [column](manifest::Column) [adapter](Adapter) that retains **only** items [matching](BitMatch)
/// at least one candidate from a [collection](std::collections).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
// NOTE: this struct is used for both "mask" and "iter" adapter chains
pub struct OneOf<'d, S, I>
where
    S: Source<'d>,
    I: schema::BitMatch,
{
    /// The wrapped data [`Source`] which yields [deserialized](Deserialize) items.
    source: S,
    /// Immutable [slice][1] over the **unsorted** candidate item [collection](std::collections).
    ///
    /// [1]: https://doc.rust-lang.org/book/ch04-03-slices.html
    items: Box<[I]>,
    /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
    phantom: PhantomData<&'d Mmap>,
}

impl<'d, S, I> Deref for OneOf<'d, S, I>
where
    S: Source<'d>,
    I: schema::BitMatch,
{
    type Target = Src<'d>;

    fn deref(&self) -> &Src<'d> {
        &self.source
    }
}

/// A [column](manifest::Column) [adapter](Adapter) that retains **only** items [matching](BitMatch)
/// at least one candidate from an ordered [collection](std::collections).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
// NOTE: this struct is used for both "mask" and "iter" adapter chains
pub struct OneOfSorted<'d, S, I>
where
    S: Source<'d>,
    I: Ord,
{
    /// The wrapped data [`Source`] which yields [deserialized](Deserialize) items.
    source: S,
    /// Immutable [slice][1] over the **sorted** candidate item [collection](std::collections) in
    /// **ascending order**.
    ///
    /// [1]: https://doc.rust-lang.org/book/ch04-03-slices.html
    items: Box<[I]>,
    /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
    phantom: PhantomData<&'d Mmap>,
}

impl<'d, S, I> Deref for OneOfSorted<'d, S, I>
where
    S: Source<'d>,
    I: Ord,
{
    type Target = Src<'d>;

    fn deref(&self) -> &Src<'d> {
        &self.source
    }
}

/// A [column](manifest::Column) [adapter](Adapter) that retains **only** items [present](BitMatch)
/// in the specified [hashed](std::hash::Hasher) candidate [set](HashSet).
///
/// [1]: manifest::Column
#[derive(Clone, Debug)]
// NOTE: this struct is used for both "mask" and "iter" adapter chains
pub struct OneOfSet<'d, S, I>
where
    S: Source<'d>,
    I: Eq + Hash,
{
    /// The wrapped data [`Source`] which yields [deserialized](Deserialize) items.
    source: S,
    /// The [hashed][1] candidate [set](HashSet) probed for each [deserialized](Deserialize) item.
    ///
    /// [1]: std::hash::Hasher
    items: HashSet<I, Xxh3Builder>,
    /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
    phantom: PhantomData<&'d Mmap>,
}

impl<'d, S, I> Deref for OneOfSet<'d, S, I>
where
    S: Source<'d>,
    I: Eq + Hash,
{
    type Target = Src<'d>;

    fn deref(&self) -> &Src<'d> {
        &self.source
    }
}

/// A [column](manifest::Column) [adapter](Adapter) that discards [`None`] items.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct IsSome<'d, S, I>
where
    S: Source<'d>,
    I: Read<'d>,
{
    /// The wrapped data [`Source`] which yields [deserialized](Deserialize) items.
    source: S,
    /// Zero-sized **marker** carrying the flattened [`Some`] type and [`Query`] lifetime.
    phantom: PhantomData<&'d I>,
}

impl<'d, S, I> Deref for IsSome<'d, S, I>
where
    S: Source<'d>,
    I: Read<'d>,
{
    type Target = Src<'d>;

    fn deref(&self) -> &Src<'d> {
        &self.source
    }
}

/// A [column](manifest::Column) [adapter](Adapter) that retains only [`None`] items.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct IsNone<'d, S>
where
    S: Source<'d>,
{
    /// The wrapped data [`Source`] which yields [deserialized](Deserialize) items.
    source: S,
    /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
    phantom: PhantomData<&'d Mmap>,
}

impl<'d, S> Deref for IsNone<'d, S>
where
    S: Source<'d>,
{
    type Target = Src<'d>;

    fn deref(&self) -> &Src<'d> {
        &self.source
    }
}

/* ---------------------------------------------------------------------------------- Join Nodes */

/// A set intersection `∩` **node** returning items from `A` [`and`](Join::and) `B`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
#[non_exhaustive] // reject external struct literal construction
pub struct Conjunct<A, B> {
    /// A single [`Adapter`] or nested combination.
    pub a: A,
    /// A single [`Adapter`] or nested combination.
    pub b: B,
}

impl<A, B> Deref for Conjunct<A, B>
where
    A: Deref,
{
    type Target = A::Target;

    fn deref(&self) -> &A::Target {
        // NOTE: a and b must originate from the same Query; equality check at construction
        &self.a
    }
}

/// A set union `∪` **node** returning items from `A` [`or`](Join::or) `B`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
#[non_exhaustive] // reject external struct literal construction
pub struct Disjunct<A, B> {
    /// A single [`Adapter`] or nested combination.
    pub a: A,
    /// A single [`Adapter`] or nested combination.
    pub b: B,
}

impl<A, B> Deref for Disjunct<A, B>
where
    A: Deref,
{
    type Target = A::Target;

    fn deref(&self) -> &A::Target {
        // NOTE: a and b must originate from the same Query; equality check at construction
        &self.a
    }
}

/// A symmetric difference `△` **node** returning items from `A` [`xor`](Join::xor) `B`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
#[non_exhaustive] // reject external struct literal construction
pub struct Adjunct<A, B> {
    /// A single [`Adapter`] or nested combination.
    pub a: A,
    /// A single [`Adapter`] or nested combination.
    pub b: B,
}

impl<A, B> Deref for Adjunct<A, B>
where
    A: Deref,
{
    type Target = A::Target;

    fn deref(&self) -> &A::Target {
        // NOTE: a and b must originate from the same Query; equality check at construction
        &self.a
    }
}

/* --------------------------------------------------------------------- Source Trait Definition */

/// A [data source](Src) or [adapter chain](Adapter) over one [`Column`](manifest::Column) that
/// yields the specified [`Item`](Self::Item) type.
///
/// ### Lifetime
///
/// This trait carries a `'d` lifetime from the underlying [`Dataset`][1] to ensure no item outlives
/// the on-disk bytes from which it was [deserialized](Deserialize). This design enables zero-copy
/// reads. [`Clone`] the item to outlive `'d`.
///
/// ### Implementation
///
/// Each [`Filter`](filter::Filter) wraps the data source in a [buffer adapter](mask) that captures
/// the necessary state to assess whole [buffers](Buffer) and individual items. Successive filters
/// therefore construct a nested adapter chain. Terminal methods e.g. [`Adapter::read`] lazily
/// [resolve](mask::Resolve) the whole chain into an [item adapter](iter) chain of the same shape.
/// The query is read in two phases:
///
/// ##### Phase 1: Mask Adapters Before IO
///
/// Every chain begins from a [data source](Src) that borrows the candidate [`Buffer`] set. The
/// terminal method builds a [mask](Exclude) that initially includes every candidate buffer. This
/// mask is passed along the chain, with each adapter assessing surviving buffers against a filter
/// to exclude candidates that are provably disjoint from the requested results set. Every adapter
/// is [monomorphized][2] against the concrete item type.
///
/// Refer to the [mask adapter module documentation](mask) for more information.
///
/// ##### Phase 2: Item Adapters During IO
///
/// The finished mask is consumed by [`iter::Src`] to yield an [`Iterator`] that lazily deserializes
/// items from **only** the retained buffers. Enclosing adapters test the item and return an
/// [`Outcome`], immediately short-circuiting once the item is [excluded][3].
///
/// Refer to the [item filter module documentation](iter) for more information.
///
/// ### Guidance
///
/// Each filter is applied in the order of declaration. Filters are lazy and short-circuiting:
/// enclosing filters **never** reassess buffers that are already excluded by upstream filters.
/// Users are advised to declare more restrictive filters upstream to reduce the result set quickly
/// and minimise work for downstream filters.
///
/// This trait is implemented by single-column **mask** and **item** adapters in both phases of the
/// query lifecycle. This trait is **not** implemented by [combined](Combine) adapters which join
/// multiple columns.
///
/// [1]: crate::dataset::Dataset
/// [2]: https://rustc-dev-guide.rust-lang.org/backend/monomorph.html
/// [3]: Outcome::exclude
pub trait Source<'d>: Deref<Target = Src<'d>> {
    /// The [deserialized](Deserialize) item type [read](Read) by the chain.
    type Item: Read<'d> + 'd;
}

/* ----------------------------------------------------------------- Source Trait Implementation */

impl<'d, I> Source<'d> for Column<'d, I>
where
    I: Read<'d> + Clone + 'd,
{
    type Item = I;
}

impl<'d, S, F, I> Source<'d> for Filter<'d, S, F, I>
where
    S: Source<'d>,
    F: Fn(&I) -> bool,
{
    type Item = S::Item;
}

impl<'d, S, B, I> Source<'d> for Range<'d, S, B, I>
where
    S: Source<'d>,
    B: RangeBounds<I>,
{
    type Item = S::Item;
}

impl<'d, S, I> Source<'d> for BitMatch<'d, S, I>
where
    S: Source<'d>,
    I: schema::BitMatch,
{
    type Item = S::Item;
}

impl<'d, S, I> Source<'d> for OneOf<'d, S, I>
where
    S: Source<'d>,
    I: schema::BitMatch,
{
    type Item = S::Item;
}

impl<'d, S, I> Source<'d> for OneOfSorted<'d, S, I>
where
    S: Source<'d>,
    I: Ord,
{
    type Item = S::Item;
}

impl<'d, S, I> Source<'d> for OneOfSet<'d, S, I>
where
    S: Source<'d>,
    I: Eq + Hash,
{
    type Item = S::Item;
}
impl<'d, S, I> Source<'d> for IsSome<'d, S, I>
where
    S: Source<'d>,
    I: Read<'d> + Clone + 'd,
{
    type Item = I;
}

impl<'d, S> Source<'d> for IsNone<'d, S>
where
    S: Source<'d>,
{
    type Item = S::Item;
}

/* -------------------------------------------------------------------- Exclude Trait Definition */

/// A [`Source`] that tests [buffers](Buffer) against a [filter](Fn) to determine inclusion.
///
/// ### Implementation
///
/// Buffer inclusion is described using a positional [mask](BitBox) where the `n`th [bit][1]
/// corresponds to the `n`th buffer from the `n`th data [segment][2].
///
/// ```text
/// buffers   [ A ][ B ][ C ][ D ][ E ]    Immutable borrowed buffer set.
/// mask        1    0    1    1    0      Mutable owned bitmask.
///             ▼         ▼    ▼
/// read        A         C    D           Buffers B and E are never read.
/// ```
///
/// Each [`Filter`] is applied subtractively to reduce the mask. Refer to the [`Src::try_exclude`]
/// documentation for the underlying iteration method.
///
/// [1]: bitvec::ptr::BitPtr
/// [2]: crate::segment::Segment
pub(crate) trait Exclude<'d>: Source<'d> + Sized {
    /// An [excluder](Exclude) method that applies a fallible [filter](Fn) to each [`Buffer`]
    /// descriptor without [detailed](Buffer::Detailed) variant exclusion using statistics.
    ///
    /// ### Guidance
    ///
    /// Use [`Exclude::with_min_max`] for filters that **can** assess detailed candidates.
    ///
    /// ### Errors
    ///
    /// - Returns [`io::Error`] if a [compact][1] item cannot be read from the [memory map](Mmap).
    /// - Forwards [`io::Error`] from the fallible `test` function.
    ///
    /// Refer to the [`Src::try_exclude`] documentation for the underlying iteration method.
    ///
    /// [1]: Buffer::Compact
    fn with_item<I, F, S>(&self, mask: &mut BitBox, filter: F) -> Result<usize, io::Error>
    where
        Self::Item: Evaluate<I> + Read<'d, Src = S>,
        S: Deserialize<'d, Ok = S> + Reader<'d, Self::Item>,
        F: Fn(&I) -> bool,
    {
        self.try_exclude(mask, |buf, mmap| {
            if let Buffer::Compact { .. } = buf {
                let mut bytes = buf.sector().slice(mmap)?;
                let keep = S::deserialize(&mut bytes)?.one()?.evaluate(&filter);
                Ok(keep)
            } else {
                Ok(true)
            }
        })
    }

    /// An [excluder](Exclude) method that applies a fallible [filter](Fn) to each [`Buffer`]
    /// descriptor with [detailed](Buffer::Detailed) variant exclusion using statistics.
    ///
    /// ### Guidance
    ///
    /// Use [`Exclude::with_item`] for filters that **cannot** assess detailed candidates.
    ///
    /// ### Errors
    ///
    /// - Returns [`io::Error`] if a [compact][1] item cannot be read from the [memory map](Mmap).
    /// - Forwards [`io::Error`] from the fallible `test` function.
    ///
    /// Refer to the [`Src::try_exclude`] documentation for the underlying iteration method.
    ///
    /// [1]: Buffer::Compact
    fn with_min_max<I, F, O, S>(&self, mask: &mut BitBox, f: F, op: O) -> Result<usize, io::Error>
    where
        Self::Item: Evaluate<I> + Read<'d, Src = S>,
        S: Deserialize<'d, Ok = S> + Reader<'d, Self::Item>,
        I: for<'de> Deserialize<'de, Ok = I> + 'd,
        F: Fn(&I) -> bool,
        O: Fn(&I, &I) -> bool,
    {
        self.try_exclude(mask, |buf, mmap| {
            if let Buffer::Detailed { min, max, .. } = buf {
                let min: I = min.slice(mmap)?.deserialize_into()?;
                let max: I = max.slice(mmap)?.deserialize_into()?;
                let keep = op(&min, &max);
                Ok(keep)
            } else if let Buffer::Compact { .. } = buf {
                let mut bytes = buf.sector().slice(mmap)?;
                let keep = S::deserialize(&mut bytes)?.one()?.evaluate(&f);
                Ok(keep)
            } else {
                Ok(true) // retain Buffer::Basic
            }
        })
    }
}

/* ---------------------------------------------------------------- Exclude Trait Implementation */

impl<'d, S> Exclude<'d> for S where S: Source<'d> + Sized {}

/* ------------------------------------------------------------------------ Buffer Filter Module */

pub mod mask {

    use std::marker::PhantomData;
    use std::ops::{Deref, Not};

    use bitvec::boxed::BitBox;

    use super::*;
    use crate::io::Deserialize;
    use crate::read::{Evaluate, IsOption, Read};

    /* -------------------------------------------------------------------------- Public Exports */

    /// A [column](manifest::Column) [adapter](Adapter) that skips the first `n` items.
    ///
    /// This adapter is initialised via [`Adapter::skip`] and excludes any [buffers](Buffer) that
    /// are provably disjoint from the requested result set.
    #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
    pub struct Skip<'d, S>
    where
        S: Adapter<'d>,
    {
        /// The wrapped [`Adapter`] that yields [deserialized](Deserialize) items.
        pub(super) source: S,
        /// The number of items to [`skip`](Adapter::skip).
        pub(super) skip: usize,
        /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
        pub(super) phantom: PhantomData<&'d Mmap>,
    }

    impl<'d, S> Deref for Skip<'d, S>
    where
        S: Adapter<'d>,
    {
        type Target = Src<'d>;

        fn deref(&self) -> &Src<'d> {
            &self.source
        }
    }

    /// A [column](manifest::Column) [adapter](Adapter) that reads at most `n` items.
    ///
    /// This adapter is initialised via [`Adapter::take`] and excludes any [buffers](Buffer) that
    /// are provably disjoint from the requested result set.
    #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
    pub struct Take<'d, S>
    where
        S: Adapter<'d>,
    {
        /// The wrapped [`Adapter`] that yields [deserialized](Deserialize) items.
        pub(super) source: S,
        /// The number of items to [`take`](Adapter::take).
        pub(super) take: usize,
        /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
        pub(super) phantom: PhantomData<&'d Mmap>,
    }

    impl<'d, S> Deref for Take<'d, S>
    where
        S: Adapter<'d>,
    {
        type Target = Src<'d>;

        fn deref(&self) -> &Src<'d> {
            &self.source
        }
    }

    /// A [column][1] [adapter](Adapter) retaining only items from `S` that are also present in `K`.
    ///
    /// [1]: manifest::Column
    #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
    pub struct SemiJoin<'d, S, K>
    where
        S: Adapter<'d>,
        K: Adapter<'d>,
    {
        /// The data [`Source`] that yields [deserialized](Deserialize) items restricted by `K`.
        pub(super) source: S,
        /// The data [`Source`] that yields [deserialized](Deserialize) items to include from `S`.
        pub(super) keys: K,
        /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
        pub(super) phantom: PhantomData<&'d Mmap>,
    }

    impl<'d, S, K> Deref for SemiJoin<'d, S, K>
    where
        S: Adapter<'d>,
        K: Adapter<'d>,
    {
        type Target = Src<'d>;

        fn deref(&self) -> &Src<'d> {
            &self.source
        }
    }

    /// A [column][1] [adapter](Adapter) retaining only items from `S` that are **not** present in
    /// `K`.
    ///
    /// [1]: manifest::Column
    #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
    pub struct AntiJoin<'d, S, K>
    where
        S: Adapter<'d>,
        K: Adapter<'d>,
    {
        /// The data [`Source`] that yields [deserialized](Deserialize) items restricted by `K`.
        pub(super) source: S,
        /// The data [`Source`] that yields [deserialized](Deserialize) items to exclude from `S`.
        pub(super) keys: K,
        /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
        pub(super) phantom: PhantomData<&'d Mmap>,
    }

    impl<'d, S, K> Deref for AntiJoin<'d, S, K>
    where
        S: Adapter<'d>,
        K: Adapter<'d>,
    {
        type Target = Src<'d>;

        fn deref(&self) -> &Src<'d> {
            &self.source
        }
    }

    impl<'d, S> Source<'d> for Skip<'d, S>
    where
        S: Adapter<'d>,
    {
        type Item = S::Item;
    }

    impl<'d, S> Source<'d> for Take<'d, S>
    where
        S: Adapter<'d>,
    {
        type Item = S::Item;
    }

    impl<'d, S, K> Source<'d> for SemiJoin<'d, S, K>
    where
        S: Adapter<'d>,
        K: Adapter<'d>,
    {
        type Item = S::Item;
    }

    impl<'d, S, K> Source<'d> for AntiJoin<'d, S, K>
    where
        S: Adapter<'d>,
        K: Adapter<'d>,
    {
        type Item = S::Item;
    }

    /* ---------------------------------------------------------------- Resolve Trait Definition */

    /// A [buffer](Buffer) [filter](Filter) chain that reduces the candidate buffer [mask](Exclude)
    /// before [resolving](Resolve::resolve) into an [item filter chain][1] of the same shape.
    ///
    /// ### Lifetime
    ///
    /// This trait carries a `'d` lifetime from the [`Dataset`][2] to ensure that no item outlives
    /// the file from which it was [deserialized](Deserialize). This design enables zero-copy reads.
    /// [`Clone`] the item to outlive `'d`.
    ///
    /// ### Implementation
    ///
    /// Each [`Filter`](filter::Filter) wraps the [data source](Source) in an [`Adapter`] that
    /// captures the necessary state to assess whole [buffers](Buffer) and individual items.
    /// Successive filters therefore construct a nested adapter chain. Terminal methods e.g.
    /// [`Adapter::read`] lazily convert the whole chain into a nested [`Iterator`] chain of the
    /// same shape. This trait determines the [buffer adapter](mask) → [item adapter](iter) state
    /// transition.
    ///
    /// Refer to the [source trait documentation](Source) for more details.
    ///
    /// ### Guidance
    ///
    /// Each filter is applied in the order of declaration. Filters are lazy and short-circuiting:
    /// enclosing filters **never** reassess buffers that are already excluded by upstream filters.
    /// Users are advised to declare more restrictive filters upstream to reduce the result set
    /// quickly and minimise work for downstream filters.
    ///
    /// [1]: iter::Resolve
    /// [2]: crate::dataset::Dataset
    pub trait Resolve<'d>: Deref<Target = Src<'d>> {
        /// The [item filter chain](iter::Resolve) returned by [`resolve`](Resolve::resolve).
        type Ok;

        /// [Excludes](Exclude) candidate buffers from the [mask](BitBox) before consuming
        /// [`self`](Self) and returning an [item filter chain](iter) of the same shape.
        ///
        /// ### Errors
        ///
        /// - [`io::Error`] if an error occurs during file [`IO`](io) or item deserialization.
        /// - [`io::Error::Number`] if a recorded item count exceeds [`usize`].
        ///
        /// Refer to the [`Src::try_exclude`] documentation for the underlying iteration method.
        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error>;
    }

    /* ------------------------------------------------------------ Resolve Trait Implementation */

    impl<'d, I> Resolve<'d> for Column<'d, I>
    where
        I: Read<'d> + Clone + 'd,
    {
        type Ok = Self;

        /// [`Src`] is not a [`Filter`]; the `mask` is unaltered by definition.
        ///
        /// Read the [trait method documentation](Resolve::resolve) for more details.
        #[allow(unused_variables, reason = "query::Src includes all buffers")]
        fn resolve(self, mask: &mut BitBox) -> Result<Self, io::Error> {
            Ok(self)
        }
    }

    impl<'d, S, F, I> Resolve<'d> for Filter<'d, S, F, I>
    where
        S: Adapter<'d>,
        S::Item: Evaluate<I>,
        F: Fn(&I) -> bool + 'd,
        I: 'd,
    {
        type Ok = Filter<'d, S::Ok, F, I>;

        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let Filter { source, filter, phantom } = self;
            let source = source.resolve(mask)?;
            source.with_item(mask, &filter)?;
            Ok(Filter { source, filter, phantom })
        }
    }

    impl<'d, S, B, I> Resolve<'d> for Range<'d, S, B, I>
    where
        S: Adapter<'d>,
        S::Item: Evaluate<I>,
        B: RangeBounds<I> + 'd,
        I: for<'de> Deserialize<'de, Ok = I> + PartialOrd + 'd,
    {
        type Ok = Range<'d, S::Ok, B, I>;

        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let Range { source, bounds, phantom } = self;
            let source = source.resolve(mask)?;
            source.with_min_max(
                mask,
                |item| bounds.contains(item),
                |lb, ub| Buffer::disjoint(lb, ub, &bounds).not(),
            )?;
            Ok(Range { source, bounds, phantom })
        }
    }

    impl<'d, S, I> Resolve<'d> for BitMatch<'d, S, I>
    where
        S: Adapter<'d>,
        S::Item: Evaluate<I>,
        I: for<'de> Deserialize<'de, Ok = I> + schema::BitMatch + PartialOrd + 'd,
    {
        type Ok = BitMatch<'d, S::Ok, I>;

        /// A buffer whose recorded span excludes the candidate can hold no match.
        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let BitMatch { source, item, phantom } = self;
            let source = source.resolve(mask)?;
            source.with_min_max(
                mask,
                |i| schema::BitMatch::eq(&item, i),
                |lb, ub| lb <= &item && &item <= ub,
            )?;
            Ok(BitMatch { source, item, phantom })
        }
    }

    impl<'d, S, I> Resolve<'d> for OneOf<'d, S, I>
    where
        S: Adapter<'d>,
        S::Item: Evaluate<I>,
        I: for<'de> Deserialize<'de, Ok = I> + schema::BitMatch + PartialOrd + 'd,
    {
        type Ok = OneOf<'d, S::Ok, I>;

        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let OneOf { source, items, phantom } = self;
            let source = source.resolve(mask)?;
            source.with_min_max(
                mask,
                |item| items.iter().any(|i| schema::BitMatch::eq(item, i)),
                |lb, ub| items.iter().any(|i| lb <= i && i <= ub),
            )?;
            Ok(OneOf { source, items, phantom })
        }
    }

    impl<'d, S, I> Resolve<'d> for OneOfSorted<'d, S, I>
    where
        S: Adapter<'d>,
        S::Item: Evaluate<I>,
        I: for<'de> Deserialize<'de, Ok = I> + Ord + 'd,
    {
        type Ok = OneOfSorted<'d, S::Ok, I>;

        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let OneOfSorted { source, items, phantom } = self;
            let source = source.resolve(mask)?;
            source.with_min_max(
                mask,
                |item| items.binary_search(item).is_ok(),
                |lb, ub| items.partition_point(|i| i < lb) < items.partition_point(|i| i <= ub),
            )?;
            Ok(OneOfSorted { source, items, phantom })
        }
    }

    impl<'d, S, I> Resolve<'d> for OneOfSet<'d, S, I>
    where
        S: Adapter<'d>,
        S::Item: Evaluate<I>,
        I: for<'de> Deserialize<'de, Ok = I> + Eq + Hash + PartialOrd + 'd,
    {
        type Ok = OneOfSet<'d, S::Ok, I>;

        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let OneOfSet { source, items, phantom } = self;
            let source = source.resolve(mask)?;
            source.with_min_max(
                mask,
                |item| items.contains(item),
                |lb, ub| items.iter().any(|i| lb <= i && i <= ub),
            )?;
            Ok(OneOfSet { source, items, phantom })
        }
    }
    impl<'d, S, K> Resolve<'d> for SemiJoin<'d, S, K>
    where
        S: Adapter<'d>,
        S::Item: Evaluate<K::Item>,
        K: Adapter<'d>,
        K::Item: for<'de> Deserialize<'de, Ok = K::Item> + Ord,
    {
        type Ok = iter::SemiJoin<S::Ok, K::Item>;

        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let keys = self.keys.into_btree_set()?;
            let source = self.source.resolve(mask)?;
            source.with_min_max(
                mask,
                |i| keys.contains(i),
                |lb, ub| keys.range(lb..=ub).next().is_some(),
            )?;
            Ok(iter::SemiJoin { source, keys })
        }
    }

    impl<'d, S, K> Resolve<'d> for AntiJoin<'d, S, K>
    where
        S: Adapter<'d>,
        S::Item: Evaluate<K::Item>,
        K: Adapter<'d>,
        K::Item: Ord,
    {
        type Ok = iter::AntiJoin<S::Ok, K::Item>;

        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let keys = self.keys.into_btree_set()?;
            let source = self.source.resolve(mask)?;
            let f = |op: &K::Item| keys.contains(op).not();
            source.with_item(mask, f)?;
            Ok(iter::AntiJoin { source, keys })
        }
    }

    impl<'d, S, I> Resolve<'d> for IsSome<'d, S, I>
    where
        S: Adapter<'d>,
        S::Item: IsOption<Item = I> + Evaluate<S::Item>,
        I: Read<'d>,
    {
        type Ok = IsSome<'d, S::Ok, I>;

        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let source = self.source.resolve(mask)?;
            source.with_item(mask, S::Item::is_some)?;
            Ok(IsSome { source, phantom: PhantomData })
        }
    }

    impl<'d, S> Resolve<'d> for IsNone<'d, S>
    where
        S: Adapter<'d>,
        S::Item: IsOption + Evaluate<S::Item>,
    {
        type Ok = IsNone<'d, S::Ok>;

        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let source = self.source.resolve(mask)?;
            source.with_item(mask, S::Item::is_none)?;
            Ok(IsNone { source, phantom: PhantomData })
        }
    }

    impl<'d, S> Resolve<'d> for Skip<'d, S>
    where
        S: Adapter<'d>,
    {
        type Ok = iter::Skip<S::Ok>;

        fn resolve(mut self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let source = self.source.resolve(mask)?;
            let (buffer, origin) = mask
                .iter_mut()
                .zip(source.buffers)
                .enumerate()
                .find_map(|b| {
                    if *b.1.0 {
                        if let Some(n) = match b.1.1.count().try_into().map_err(io::Error::from) {
                            Ok(c) => self.skip.checked_sub(c),
                            Err(e) => return Err(e).into(),
                        } {
                            self.skip = n;
                            b.1.0.commit(false);
                        } else {
                            let out = (b.0, self.skip);
                            return Ok(out).into();
                        }
                    };
                    None
                })
                .transpose()?
                .unwrap_or((mask.len(), self.skip));
            Ok(iter::Skip { source, buffer, origin })
        }
    }

    impl<'d, S> Resolve<'d> for Take<'d, S>
    where
        S: Adapter<'d>,
    {
        type Ok = iter::Take<S::Ok>;

        fn resolve(mut self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let source = self.source.resolve(mask)?;
            let origin = iter::Resolve::origin(&source, mask);
            self.take = self.take.saturating_add(origin);
            let (limit, keep) = mask
                .iter()
                .by_vals()
                .zip(source.buffers)
                .enumerate()
                .find_map(|b| {
                    if b.1.0 {
                        if let Some(n) = match b.1.1.count().try_into().map_err(io::Error::from) {
                            Ok(c) => self.take.checked_sub(c).filter(|n| n > &usize::MIN),
                            Err(e) => return Err(e).into(),
                        } {
                            self.take = n;
                        } else {
                            let out = (b.0, self.take);
                            return Ok(out).into();
                        }
                    };
                    None
                })
                .transpose()?
                .inspect(|i| {
                    // NOTE: clear all buffers beyond the retained slice
                    mask.split_at_mut(i.0 + 1).1.fill(false)
                })
                .unwrap_or((mask.len(), self.take));
            Ok(iter::Take { source, limit, keep })
        }
    }

    impl<'d, A, B> Resolve<'d> for Conjunct<A, B>
    where
        A: Resolve<'d>,
        B: Resolve<'d>,
    {
        type Ok = Conjunct<A::Ok, B::Ok>;

        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let a = self.a.resolve(mask)?;
            let b = self.b.resolve(mask)?;
            Ok(Conjunct { a, b })
        }
    }

    impl<'d, A, B> Resolve<'d> for Disjunct<A, B>
    where
        A: Resolve<'d>,
        B: Resolve<'d>,
    {
        type Ok = Disjunct<A::Ok, B::Ok>;

        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let mut other = mask.clone();
            let a = self.a.resolve(&mut other)?;
            **mask ^= &*other;
            let b = self.b.resolve(mask)?;
            **mask |= &*other;
            Ok(Disjunct { a, b })
        }
    }

    impl<'d, A, B> Resolve<'d> for Adjunct<A, B>
    where
        A: Resolve<'d>,
        B: Resolve<'d>,
    {
        type Ok = Adjunct<A::Ok, B::Ok>;

        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, io::Error> {
            let Adjunct { a, b } = self;
            let united = Disjunct { a, b }.resolve(mask)?;
            Ok(Adjunct { a: united.a, b: united.b })
        }
    }
}

/* -------------------------------------------------------------------------- Item Filter Module */

pub mod iter {
    //! The **item [iterator](Iterator) adapter chain** evaluated during file [`IO`](io).

    use std::collections::BTreeSet;
    use std::iter;
    use std::ops::{Deref, Not};

    use bitvec::boxed::BitBox;

    use super::*;
    use crate::io::Error;

    /* -------------------------------------------------------------------------- Public Exports */

    /// An [`Iterator`] that lazily [deserializes](Deserialize) items from one [`Buffer`].
    enum Decode<S, I>
    where
        S: Iterator<Item = Result<I, Error>>,
        I: Clone,
    {
        /// An [`Iterator`] that repeats one item for `n` number of [iterations](Iterator::next).
        One(iter::RepeatN<I>),
        /// An [`Iterator`] that yields heterogeneous items from one [`Buffer`].
        Std(S),
        /// A [buffer](Buffer) [deserialization](Deserialize) [error](Error) yielded **once**.
        Err(iter::Once<Error>),
    }

    impl<S, I> Iterator for Decode<S, I>
    where
        S: Iterator<Item = Result<I, Error>>,
        I: Clone,
    {
        type Item = Result<I, Error>;

        fn next(&mut self) -> Option<Result<I, Error>> {
            match self {
                Self::One(i) => i.next().map(Ok),
                Self::Std(i) => i.next(),
                Self::Err(e) => e.next().map(Err),
            }
        }
    }

    /// A lazy [deserializing](Deserialize) **item source** for one [`Column`] chained across all
    /// on-disk [buffers](Buffer).
    pub(crate) struct Src<'d, B>
    where
        B: IntoIterator<Item = &'d Buffer> + 'd,
    {
        /// Retained [`Buffer`] descriptors.
        buffers: B,
        /// Read-only [memory map](Mmap) over the immutable segment region.
        mmap: &'d Mmap,
    }

    impl<'d, B> Src<'d, B>
    where
        B: IntoIterator<Item = &'d Buffer>,
    {
        pub(crate) const fn new(buffers: B, mmap: &'d Mmap) -> Self {
            Self { buffers, mmap }
        }

        /// Consumes [`self`](Src) and returns an item [`Iterator`].
        ///
        /// Items are [deserialized](Deserialize) exactly once before being [filtered](filter)
        /// through the [item terator adapter chain](iter).
        ///
        /// Refer to [`Src`] for more information.
        pub(crate) fn into_iter<I, S>(self) -> impl Iterator<Item = Result<I, Error>> + 'd
        where
            I: Read<'d, Src = S> + Clone + 'd,
            S: Deserialize<'d, Ok = S> + Reader<'d, I>,
        {
            self.buffers
                .into_iter()
                .map(|b| {
                    let n = b.count().try_into()?;
                    let mut bytes = b.sector().slice(self.mmap)?;
                    let src = S::deserialize(&mut bytes)?;
                    let reader = if let Buffer::Compact { .. } = b {
                        let item = src.one()?;
                        let iter = iter::repeat_n(item, n);
                        Decode::One(iter)
                    } else {
                        let iter = src.iter()?.take(n);
                        Decode::Std(iter)
                    };
                    Ok(reader)
                })
                .flat_map(|s| {
                    s.unwrap_or_else(|e| {
                        let err = iter::once(e);
                        Decode::Err(err)
                    })
                })
        }
    }

    /// The [resolved][1] form of [`mask`]`::`[`SemiJoin`](mask::SemiJoin) that yields
    /// [deserialized](Deserialize) items from `S` that are also present in the [`keys`][2] set.
    ///
    /// [1]: mask::Resolve::resolve
    /// [2]: BTreeSet
    #[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
    pub struct SemiJoin<S, I>
    where
        I: Ord,
    {
        /// The [resolved][1] data [`Source`] that yields [deserialized](Deserialize) items.
        ///
        /// [1]: mask::Resolve::resolve
        pub(super) source: S,
        /// Ordered distinct items to include from `S`.
        pub(super) keys: BTreeSet<I>,
    }

    impl<S, I> Deref for SemiJoin<S, I>
    where
        S: Deref,
        I: Ord,
    {
        type Target = S::Target;

        fn deref(&self) -> &Self::Target {
            &self.source
        }
    }

    impl<'d, S, I> Source<'d> for SemiJoin<S, I>
    where
        S: Source<'d>,
        I: Ord,
    {
        type Item = S::Item;
    }

    /// The [resolved][1] form of [`mask`]`::`[`AntiJoin`](mask::AntiJoin) that yields
    /// [deserialized](Deserialize) items from `S` that are **not** present in the [`keys`][2] set.
    ///
    /// [1]: mask::Resolve::resolve
    /// [2]: BTreeSet
    #[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
    pub struct AntiJoin<S, I>
    where
        I: Ord,
    {
        /// The [resolved][1] data [`Source`] that yields [deserialized](Deserialize) items.
        ///
        /// [1]: mask::Resolve::resolve
        pub(super) source: S,
        /// Ordered distinct items to exclude from `S`.
        pub(super) keys: BTreeSet<I>,
    }

    impl<S, I> Deref for AntiJoin<S, I>
    where
        S: Deref,
        I: Ord,
    {
        type Target = S::Target;

        fn deref(&self) -> &Self::Target {
            &self.source
        }
    }

    impl<'d, S, I> Source<'d> for AntiJoin<S, I>
    where
        S: Source<'d>,
        I: Ord,
    {
        type Item = S::Item;
    }

    /// The [resolved][1] form of [`mask`]`::`[`Skip`](mask::Skip) that skips the first `n` items.
    ///
    /// [1]: mask::Resolve::resolve
    #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
    pub struct Skip<S> {
        /// The [resolved][1] data [`Source`] that yields [deserialized](Deserialize) items.
        ///
        /// [1]: mask::Resolve::resolve
        pub(super) source: S,
        /// Index of the [`Buffer`] holding the first retained item.
        pub(super) buffer: usize,
        /// Index of the first retained item within the first retained [`Buffer`].
        pub(super) origin: usize,
    }

    impl<S> Deref for Skip<S>
    where
        S: Deref,
    {
        type Target = S::Target;

        fn deref(&self) -> &Self::Target {
            &self.source
        }
    }

    impl<'d, S> Source<'d> for Skip<S>
    where
        S: Source<'d>,
    {
        type Item = S::Item;
    }

    /// The [resolved][1] form of [`mask`]`::`[`Take`](mask::Take) that reads at most `n` items.
    ///
    /// [1]: mask::Resolve::resolve
    #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
    pub struct Take<S> {
        /// The [resolved][1] data [`Source`] that yields [deserialized](Deserialize) items.
        ///
        /// [1]: mask::Resolve::resolve
        pub(super) source: S,
        /// Index of the [`Buffer`] holding the last retained item.
        pub(super) limit: usize,
        /// The number of items to [`take`](super::Adapter::take) from the last retained [`Buffer`].
        pub(super) keep: usize,
    }

    impl<S> Deref for Take<S>
    where
        S: Deref,
    {
        type Target = S::Target;

        fn deref(&self) -> &Self::Target {
            &self.source
        }
    }

    impl<'d, S> Source<'d> for Take<S>
    where
        S: Source<'d>,
    {
        type Item = S::Item;
    }

    /* ----------------------------------------------------------------------------------- Tests */

    #[cfg(test)]
    mod tests {
        use std::num::NonZeroU64;

        use memmap2::MmapMut;

        use super::*;
        use crate::Serialize;
        use crate::io::Sector;

        /* ------------------------------------------------------------------------ Shared State */

        /// Build a read-only anonymous [`Mmap`] over the provided `bytes`.
        fn map(bytes: &[u8]) -> Mmap {
            let mut mmap = MmapMut::map_anon(bytes.len().max(1)).expect("Anonymous map failed");
            mmap[..bytes.len()].copy_from_slice(bytes);
            mmap.make_read_only().expect("Read-only conversion failed")
        }

        /// A [`Sector`] of `length` bytes anchored at the map origin.
        fn sector(length: usize) -> Sector {
            Sector {
                offset: u64::MIN,
                size: NonZeroU64::new(length as u64).expect("Empty body"),
            }
        }

        /// Drain a decoded buffer into an owned [`Vec`].
        fn drained(buffer: &Buffer, mmap: &Mmap) -> Result<Vec<u32>, Error> {
            Src::new(iter::once(buffer), mmap).into_iter::<u32, &[u8]>().collect()
        }

        /* -------------------------------------------------------------------------- Unit Tests */

        /// [`iter`](Src::into_iter) yields every item of a [`Basic`](Buffer::Basic) buffer,
        /// truncated to the recorded `count`.
        #[test]
        fn decode_reads_standard_buffer() {
            let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
            let mmap = map(&bytes);
            let buffer = Buffer::Basic {
                buffer: sector(bytes.len()),
                count: NonZeroU64::new(3).expect("Zero rows"),
            };
            let items = drained(&buffer, &mmap).expect("Decode failed");
            assert_eq!(items, [10, 20, 30]);
        }

        /// [`iter`](Src::into_iter) resolves the single item of a [`Compact`](Buffer::Compact)
        /// buffer exactly once and repeats it `count` times.
        #[test]
        fn decode_repeats_compact_item() {
            let bytes = vec![7u32].serialize().expect("Serialize failed");
            let mmap = map(&bytes);
            let buffer = Buffer::Compact {
                buffer: sector(bytes.len()),
                count: NonZeroU64::new(3).expect("Zero rows"),
            };
            let items = drained(&buffer, &mmap).expect("Decode failed");
            assert_eq!(items, [7, 7, 7]);
        }

        /// A sector extending beyond the memory map surfaces as the sole item the buffer yields,
        /// in place of the items it could not decode.
        #[test]
        fn decode_rejects_out_of_bounds_sector() {
            let mmap = map(&[u8::MIN; 8]);
            let buffer = Buffer::Basic {
                buffer: sector(64), // spans past the eight-byte map
                count: NonZeroU64::new(1).expect("Zero rows"),
            };
            let error = drained(&buffer, &mmap).expect_err("Out-of-bounds sector accepted");
            assert!(matches!(error, Error::Truncated { .. }));
        }
    }
}

/* ------------------------------------------------------------------------------ Specific Error */

/// Errors returned from [`Query`] construction and execution.
///
/// Enum variants cover various granular error cases that may arise when working with queries.
/// Users should consider handling errors explicitly wherever possible to provide meaningful
/// error messages and recovery actions.
///
/// ### Implementation
///
/// This enum is `#[non_exhaustive]` meaning additional variants may be added in future versions.
/// Implementers are advised to include a wildcard arm `_` to account for potential additions.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug)]
#[non_exhaustive] // accommodate potential future error cases
pub enum Error {
    /// The requested [`Column`] name was not found in the query [`BTreeMap`].
    Column {
        /// The requested [`Column`] name.
        name: String,
    },
    /// Underlying [`io::Error`] from the [msca](crate) file.
    Io(io::Error),
    /// Underlying [`number::Error`] from a numerical operation or conversion.
    Number(number::Error),
    /// The requested [`Type`] did not match the actual on-disk [`Column`] type.
    Type {
        /// The [`Type`] expected by the caller.
        expect: Type,
        /// The actual on-disk column [`Type`].
        actual: Type,
    },
    /// Attempted to combine two handles that do not share one parent [`Query`].
    ///
    /// A combination reconciles a single buffer selection across both legs, so [`and`](Join::and),
    /// [`or`](Join::or) and [`xor`](Join::xor) require both legs to read the same memory map and
    /// expose the same [`Schema`].
    ///
    /// ### Guidance
    ///
    /// Use [`semi_join`](filter::Filter::semi_join) or [`anti_join`](filter::Filter::anti_join) to
    /// filter a [`Column`] against a column of a separate query, which may belong to another
    /// [`Dataset`](crate::Dataset) entirely. The other column is drained once to a sorted key set
    /// rather than reconstructed, so the result carries the filtered column alone and no
    /// cross-schema item is rebuilt.
    ///
    /// Refer to the [module-level documentation](self) for more details.
    Join,
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Column { name } => write!(f, "Column '{name}' not found"),
            Self::Io(e) => write!(f, "Query IO error → {e}"),
            Self::Number(e) => write!(f, "Number error → {e}"),
            Self::Type { expect, actual } => write!(f, "Type error → {expect} ≠ {actual}"),
            Self::Join => write!(f, "Join error → handles from different queries"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(src: io::Error) -> Self {
        match src {
            io::Error::Number(e) => e.into(), // Flatten number error nesting
            other => Self::Io(other),
        }
    }
}

impl From<number::Error> for Error {
    fn from(e: number::Error) -> Self {
        Self::Number(e)
    }
}

impl From<TryFromIntError> for Error {
    fn from(e: TryFromIntError) -> Self {
        number::Error::from(e).into()
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        io::Error::from(e).into()
    }
}

//noinspection DuplicatedCode → Conversion is implemented for error types in different modules.
impl<T, E> From<Error> for Result<T, E>
where
    E: From<Error>,
{
    fn from(error: Error) -> Self {
        Err(E::from(error))
    }
}

/* --------------------------------------------------------------------------------------- Tests */

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::ops::Not;

    use bitvec::vec::BitVec;
    use memmap2::MmapMut;

    use super::filter::{Filter, IsNone, IsSome};
    use super::{Adapter, Join, *};
    use crate::accumulate::{Accumulate, OptBitVec, OptInSitu, Seq};
    use crate::{Sector, Serialize};

    /* ---------------------------------------------------------------------------- Shared State */

    /// Collect the [`Include`](Outcome::Include) items from a stream, dropping
    /// [`Exclude`](Outcome::Exclude) and panicking on a failed chain construction or any
    /// [`Error`](Outcome::Error).
    fn collected<I, S, E>(stream: Result<S, E>) -> Vec<I>
    where
        S: Iterator<Item = Outcome<I>>,
        E: fmt::Debug,
    {
        stream
            .expect("Stream construction failed")
            .filter_map(|outcome| match outcome {
                Outcome::Include(item) => item.into(),
                Outcome::Exclude(..) => None,
                Outcome::Absent => None,
                Outcome::Error(error) => panic!("Read error → {error}"),
            })
            .collect()
    }

    /// A [`Sector`] spanning the one `u32` at element `slot` of a serialized body.
    fn slot(index: u64) -> Sector {
        let width = size_of::<u32>() as u64;
        Sector::new(index * width, width).expect("Sector::new failed")
    }

    /// Build a [`Detailed`](Buffer::Detailed) `u32` descriptor over a serialized body,
    /// pointing `min` and `max` at the real items held at those element slots.
    fn detailed(len: usize, count: u64, min: u64, max: u64) -> Buffer {
        Buffer::Detailed {
            buffer: Sector {
                offset: u64::MIN,
                size: NonZeroU64::new(len as u64).expect("Empty body"),
            },
            count: NonZeroU64::new(count).expect("Zero rows"),
            min: slot(min),
            max: slot(max),
        }
    }

    /// Owns the mapped bytes and the on-disk column descriptors that a borrowed [`Query`] reads.
    ///
    /// A `Query` borrows the manifest, so a test binds the fixture first and derives the query
    /// from it — mirroring how [`Dataset::query`](crate::Dataset::query) borrows a live dataset.
    struct Fixture {
        mmap: Mmap,
        schema: manifest::Schema,
    }

    impl Fixture {
        /// Map `bytes` and register the provided on-disk [columns](manifest::Column) by name.
        fn new<C>(bytes: &[u8], columns: C) -> Self
        where
            C: std::iter::IntoIterator<Item = (String, manifest::Column)>,
        {
            let mut mmap = MmapMut::map_anon(bytes.len().max(1)).expect("Anonymous map failed");
            mmap[..bytes.len()].copy_from_slice(bytes);
            let read = mmap.make_read_only().expect("Read-only conversion failed");
            let columns = std::iter::IntoIterator::into_iter(columns).collect();
            let sector = Sector::new(u64::MIN, NonZeroU64::MIN).expect("Sector::new failed");
            Fixture {
                mmap: read,
                schema: manifest::Schema { sector, columns },
            }
        }

        /// Borrow the fixture as a [`Query`] over every registered column.
        fn query<'d>(&'d self) -> Query<'d> {
            Query { mmap: &self.mmap, schema: &self.schema }
        }
    }

    /// An on-disk [`Column`](manifest::Column) of `ty` spanning the provided [buffers](Buffer).
    fn column<B>(name: &str, ty: Type, buffers: B) -> (String, manifest::Column)
    where
        B: std::iter::IntoIterator<Item = Buffer>,
    {
        let column = manifest::Column {
            ty,
            buffers: std::iter::IntoIterator::into_iter(buffers).collect(),
        };
        (String::from(name), column)
    }

    /// A [`Basic`](Buffer::Basic) descriptor of `count` items spanning `len` bytes at `offset`.
    fn span(offset: usize, len: usize, count: usize) -> Buffer {
        Buffer::Basic {
            buffer: Sector::new(offset as u64, len as u64).expect("Sector::new failed"),
            count: NonZeroU64::new(count as u64).expect("Zero rows"),
        }
    }

    /// Build a single-column `u32` [`Fixture`] named `v` whose descriptor carries real statistics.
    fn stats(items: &[u32]) -> Fixture {
        let bytes = items.to_vec().serialize().expect("Serialize failed");
        let last = items.len() as u64 - 1;
        let buffer = detailed(bytes.len(), items.len() as u64, u64::MIN, last);
        with(&bytes, Type::U32, buffer)
    }

    /// Build a single-column [`Fixture`] named `v` over the provided bytes and [`Buffer`].
    fn with(bytes: &[u8], ty: Type, buffer: Buffer) -> Fixture {
        Fixture::new(bytes, [column("v", ty, [buffer])])
    }

    /// Build a single-column [`Fixture`] named `v` over the provided serialized bytes; the
    /// descriptor is [`Basic`](Buffer::Basic), so it carries no statistics and is never pruned.
    fn query(bytes: &[u8], ty: Type, count: u64) -> Fixture {
        let buffer = span(usize::MIN, bytes.len(), count as usize);
        with(bytes, ty, buffer)
    }

    /// Build a single-column [`Fixture`] named `v` whose descriptor is a
    /// [`Compact`](Buffer::Compact) buffer spanning one repeated item.
    fn compact(bytes: &[u8], ty: Type, count: u64) -> Fixture {
        let buffer = Buffer::Compact {
            buffer: Sector::new(u64::MIN, bytes.len() as u64).expect("Sector::new failed"),
            count: NonZeroU64::new(count).expect("Zero rows"),
        };
        with(bytes, ty, buffer)
    }

    /// Build a two-column [`Fixture`] (`a` then `b`) with a distinct `u32` [`Buffer`] per column.
    fn pair(a: &[u32], b: &[u32]) -> Fixture {
        let ab = a.to_vec().serialize().expect("Serialize a");
        let bb = b.to_vec().serialize().expect("Serialize b");
        let left = column("a", Type::U32, [span(usize::MIN, ab.len(), a.len())]);
        let right = column("b", Type::U32, [span(ab.len(), bb.len(), b.len())]);
        let bytes = [ab, bb].concat();
        Fixture::new(&bytes, [left, right])
    }

    /// Build a single-column [`Fixture`] named `v` whose items span two [`Basic`](Buffer::Basic)
    /// buffers, one per data segment, mirroring a column written across two write cycles.
    fn spread(a: &[u32], b: &[u32]) -> Fixture {
        let ab = a.to_vec().serialize().expect("Serialize a");
        let bb = b.to_vec().serialize().expect("Serialize b");
        let first = span(usize::MIN, ab.len(), a.len());
        let second = span(ab.len(), bb.len(), b.len());
        let bytes = [ab, bb].concat();
        Fixture::new(&bytes, [column("v", Type::U32, [first, second])])
    }

    /// A two-buffer `u32` [`Fixture`] named `v`: a [`Compact`](Buffer::Compact) run of `one`
    /// repeated `count` times, followed by a [`Basic`](Buffer::Basic) buffer holding `items`.
    ///
    /// The compact head is what a later filter can prune **exactly**, which is what makes this the
    /// fixture for the positional window anchoring tests.
    fn mixed(one: u32, count: u64, items: &[u32]) -> Fixture {
        let head = vec![one].serialize().expect("Serialize head failed");
        let tail = items.to_vec().serialize().expect("Serialize tail failed");
        let first = Buffer::Compact {
            buffer: Sector::new(u64::MIN, head.len() as u64).expect("Sector::new failed"),
            count: NonZeroU64::new(count).expect("Zero rows"),
        };
        let second = span(head.len(), tail.len(), items.len());
        let bytes = [head, tail].concat();
        Fixture::new(&bytes, [column("v", Type::U32, [first, second])])
    }

    /// A [`Compact`](Buffer::Compact) descriptor of `count` repetitions spanning `len` bytes at
    /// `offset`; the single item is resolved exactly, so any item filter prunes the buffer.
    fn repeat(offset: usize, len: usize, count: u64) -> Buffer {
        Buffer::Compact {
            buffer: Sector::new(offset as u64, len as u64).expect("Sector::new failed"),
            count: NonZeroU64::new(count).expect("Zero rows"),
        }
    }

    /// A three-column `u32` [`Fixture`] (`a`, `b`, `c`) holding one exactly prunable
    /// [`Compact`](Buffer::Compact) buffer per segment, three segments deep.
    fn segmented(items: [[u32; 3]; 3]) -> Fixture {
        let mut bytes = Vec::new();
        let mut columns = Vec::with_capacity(items.len());
        for pair in ["a", "b", "c"].into_iter().zip(items) {
            let mut buffers = Vec::with_capacity(pair.1.len());
            for item in pair.1 {
                let body = vec![item].serialize().expect("Serialize failed");
                buffers.push(repeat(bytes.len(), body.len(), 2));
                bytes.extend_from_slice(&body); // NOTE: Serialize::extend shadows Extend::extend
            }
            columns.push(column(pair.0, Type::U32, buffers));
        }
        Fixture::new(&bytes, columns)
    }

    /// A composite item rebuilt from the single `v` column; a hand-written stand-in for the type
    /// that `#[derive(Read)]` generates, which is unavailable inside this crate.
    struct Composed {
        v: u32,
    }

    /// The composite reader for [`Composed`], holding one boxed column stream per field.
    ///
    /// `N` carries the shape of the resolved combination, which is what lets the per-slot fold
    /// fold monomorphize. A single field folds nothing, so it is unconstrained here.
    struct Rebuild<'a, N> {
        v: Box<dyn Iterator<Item = Outcome<u32>> + 'a>,
        src: PhantomData<N>,
    }

    impl<'a, S> Composite<'a, S> for Composed
    where
        S: mask::Resolve<'a, Ok: iter::Resolve<'a, Item = u32>>,
    {
        type Reader = Rebuild<'a, S::Ok>;

        fn new(src: S) -> Result<Self::Reader, Error> {
            let mut mask = Src::mask(&src);
            let v = mask::Resolve::resolve(src, &mut mask)?;
            let v = Box::new(iter::Resolve::resolve(v, mask)?);
            Ok(Rebuild { v, src: PhantomData })
        }
    }

    impl<'a, N> Iterator for Rebuild<'a, N> {
        type Item = Outcome<Composed>;

        fn next(&mut self) -> Option<Outcome<Composed>> {
            let v = self.v.next()?;
            let (keep, v) = match v {
                Outcome::Include(item) => (true, item),
                Outcome::Exclude(item) => (false, item),
                Outcome::Error(e) => return Outcome::Error(e).into(), // nothing to rebuild from
                Outcome::Absent => return Outcome::Absent.into(),
            };
            let item = Composed { v };
            let outcome = match keep {
                true => Outcome::Include(item),
                false => Outcome::Exclude(item),
            };
            outcome.into()
        }
    }

    impl<'a> Unfiltered<'a> for Composed {
        type Reader = Rebuild<'a, Column<'a, u32>>;

        fn unfiltered(query: Query<'a>) -> Result<Self::Reader, Error> {
            let v = query.column::<u32>("v")?;
            Composed::new(v)
        }

        /// Mirrors the derived body: window the column, settle the selection, then build.
        fn nth(q: Query<'a>, n: usize) -> Result<Self::Reader, Error> {
            let v = q.column::<u32>("v")?;
            let v = Adapter::skip(v, n);
            let v = Adapter::take(v, 1usize);
            let mut mask = Src::mask(&v);
            let v = mask::Resolve::resolve(v, &mut mask)?;
            let origin = iter::Resolve::origin(&v, &mask);
            let v = Box::new(iter::Resolve::resolve(v, mask)?.skip(origin));
            Ok(Rebuild { v, src: PhantomData })
        }
    }

    /* ------------------------------------------------------------------------------ Unit Tests */

    /// A plain `u32` column streams every committed item back in order.
    #[test]
    fn column_round_trips_u32() {
        let data: Vec<u32> = vec![10, 20, 30];
        let bytes = data.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 3);
        let query = query.query();
        let rows = collected(query.column::<u32>("v").expect("Column failed").read());
        assert_eq!(rows, data);
    }

    /// [`Query::read`] yields the same items as [`column`](Query::column) followed by
    /// [`read`](Adapter::read), and reports the same error for an absent name.
    ///
    /// The two functions construct the stream by separate routes, so a change to either alone
    /// breaks the equality asserted here. Both a single buffer and a column spread across two
    /// segments are covered, because the two routes enumerate the buffer set differently.
    #[test]
    fn query_read_matches_the_column_terminal() {
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        let fixture = query(&bytes, Type::U32, 3);
        let one = fixture.query();
        let single = collected(one.read::<u32>("v"));
        let chained = collected(one.column::<u32>("v").expect("Column failed").read());
        let absent = one.read::<u32>("missing").err(); // opaque Ok, so no expect_err
        let fixture = spread(&[10, 20, 30, 40], &[50, 60, 70, 80]);
        let many = fixture.query();
        let across = collected(many.read::<u32>("v"));
        let walked = collected(many.column::<u32>("v").expect("Column failed").read());
        assert_eq!(single, [10, 20, 30]);
        assert_eq!(single, chained);
        assert!(matches!(absent, Some(Error::Column { .. })));
        assert_eq!(across, [10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(across, walked); // the buffer set is enumerated identically by both routes
    }

    /// Items are bound to the mapped bytes, so a handle outlives the [`Query`] that opened it.
    ///
    /// The query is dropped before the column is read, which only compiles while the item lifetime
    /// tracks the [`Dataset`](crate::Dataset) rather than the query handle.
    #[test]
    fn items_outlive_the_query_handle() {
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        let fixture = query(&bytes, Type::U32, 3);
        let handle = {
            let query = fixture.query();
            query.column::<u32>("v").expect("Column failed")
        };
        assert_eq!(collected(handle.read()), [10, 20, 30]);
    }

    /// A column extracted at the wrong type is rejected with [`Error::Type`].
    #[test]
    fn column_type_mismatch_errors() {
        let bytes = vec![1u32].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 1);
        let query = query.query();
        let error = query.column::<u16>("v").expect_err("Type mismatch accepted");
        assert!(matches!(error, Error::Type { .. }));
    }

    /// A [`Compact`](manifest::Buffer::Compact) descriptor decodes its single item once and repeats
    /// it exactly `count` times without further file access.
    #[test]
    fn compact_column_decodes_once() {
        let bytes = vec![7u32].serialize().expect("Serialize failed");
        let query = compact(&bytes, Type::U32, 3);
        let query = query.query();
        let rows = collected(query.column::<u32>("v").expect("Column failed").read());
        assert_eq!(rows, [7, 7, 7]);
    }

    /// A range **containing** the item of a [`Compact`](Buffer::Compact) column retains the
    /// buffer and repeats the item; a disjoint range prunes it eagerly instead of repeating an
    /// [`Exclude`](Outcome::Exclude) outcome to exhaust it.
    #[test]
    fn compact_repeats_contained_range() {
        let bytes = vec![7u32].serialize().expect("Serialize failed");
        let query = compact(&bytes, Type::U32, 3);
        let query = query.query();
        let handle = query.column::<u32>("v").expect("Column failed").range(5u32..10);
        assert_eq!(collected(handle.read()), [7, 7, 7]);
    }

    /// Every item filter evaluates a [`Compact`](manifest::Buffer::Compact) item **exactly** at
    /// query time, so a provably excluded compact buffer is pruned before any streaming.
    #[test]
    fn compact_prunes_disjoint_item() {
        let bytes = vec![7u32].serialize().expect("Serialize failed");
        let away = compact(&bytes, Type::U32, 3);
        let away = away.query();
        let away = away.column::<u32>("v").expect("Column failed").eq(100u32);
        assert!(collected(away.read()).is_empty());
        let kept = compact(&bytes, Type::U32, 3);
        let kept = kept.query();
        let kept = kept.column::<u32>("v").expect("Column failed").eq(7u32);
        assert_eq!(collected(kept.read()), [7, 7, 7]);
    }

    /// Inequality proves nothing from a statistic range, but prunes a
    /// [`Compact`](manifest::Buffer::Compact) buffer whose item is bit-identical to the operand.
    #[test]
    fn compact_prunes_ne() {
        let bytes = vec![7u32].serialize().expect("Serialize failed");
        let away = compact(&bytes, Type::U32, 3);
        let away = away.query();
        let away = away.column::<u32>("v").expect("Column failed").ne(7u32);
        assert!(collected(away.read()).is_empty());
        let kept = compact(&bytes, Type::U32, 3);
        let kept = kept.query();
        let kept = kept.column::<u32>("v").expect("Column failed").ne(9u32);
        assert_eq!(collected(kept.read()), [7, 7, 7]);
    }

    /// A [`Basic`](Buffer::Basic) buffer carries no statistics: a range filter retains the buffer
    /// and filters the items at read time instead.
    #[test]
    fn basic_streams_unpruned() {
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 3);
        let query = query.query();
        let handle = query.column::<u32>("v").expect("Column failed").range(15u32..25);
        assert_eq!(collected(handle.read()), [20]); // filtered at read time
    }

    /// A compact `String` column resolves its framed composite item through the reader pipeline, so
    /// item filters prune and retain it correctly.
    #[test]
    fn string_filters_prune_compact() {
        let bytes = {
            let mut acc = Seq::<u8>::default();
            acc.push(String::from("red"));
            acc.serialize().expect("Serialize failed")
        };
        let away = compact(&bytes, Type::String, 3);
        let away = away.query();
        let away = away.column::<String>("v").expect("Column failed").eq(String::from("blue"));
        assert!(collected(away.read()).is_empty());
        let kept = compact(&bytes, Type::String, 3);
        let kept = kept.query();
        let kept = kept.column::<String>("v").expect("Column failed").eq(String::from("red"));
        assert_eq!(collected(kept.read()).len(), 3);
    }

    /// A bit-packed `bool` column streams every committed item back in order.
    #[test]
    fn bool_column_round_trip() {
        let mut acc = BitVec::default();
        [true, false, true].into_iter().for_each(|bit| acc.push(bit));
        let bytes = acc.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::Bool, 3);
        let query = query.query();
        let rows = collected(query.column::<bool>("v").expect("Column failed").read());
        assert_eq!(rows, vec![true, false, true]);
    }

    /// An [`OptBitVec`] `Option<u32>` column round-trips every `Some` and `None` item.
    #[test]
    fn opt_bit_vec_column_round_trip() {
        let mut acc = OptBitVec::<u32>::default();
        [Some(1u32), None, Some(3)].into_iter().for_each(|v| acc.push(v));
        let bytes = acc.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::option(Type::U32), 3);
        let query = query.query();
        let rows = collected(query.column::<Option<u32>>("v").expect("Column failed").read());
        assert_eq!(rows, vec![Some(1), None, Some(3)]);
    }

    /// A niche-optimised `Option<NonZeroU64>` column round-trips every item.
    #[test]
    fn niche_option_column_round_trip() {
        let mut acc = OptInSitu::<NonZeroU64>::default();
        [NonZeroU64::new(5), None, NonZeroU64::new(7)].into_iter().for_each(|v| acc.push(v));
        let bytes = acc.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::option(Type::NZU64), 3);
        let query = query.query();
        let rows =
            collected(query.column::<Option<NonZeroU64>>("v").expect("Column failed").read());
        assert_eq!(rows, vec![NonZeroU64::new(5), None, NonZeroU64::new(7)]);
    }

    /// An unsized `Vec<u8>` column round-trips each variable-length item.
    #[test]
    fn seq_column_round_trip() {
        let mut acc = Seq::<u8>::default();
        acc.push(vec![97, 98, 99]);
        acc.push(vec![100, 101]);
        let bytes = acc.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::sequence(Type::U8), 2);
        let query = query.query();
        let rows = collected(query.column::<Vec<u8>>("v").expect("Column failed").read());
        assert_eq!(rows, vec![vec![97, 98, 99], vec![100, 101]]);
    }

    /// A `String` column round-trips each owned UTF-8 item.
    #[test]
    fn string_column_round_trip() {
        let mut acc = Seq::<u8>::default();
        acc.push("héllo".as_bytes().to_vec());
        acc.push("xyz".as_bytes().to_vec());
        let bytes = acc.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::String, 2);
        let query = query.query();
        let rows = collected(query.column::<String>("v").expect("Column failed").read());
        assert_eq!(rows, vec![String::from("héllo"), String::from("xyz")]);
    }

    /// A `&str` column borrows each item directly from the memory map, zero-copy.
    #[test]
    fn str_column_zero_copy() {
        let mut acc = Seq::<u8>::default();
        acc.push(b"abc".to_vec());
        acc.push(b"de".to_vec());
        let bytes = acc.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::String, 2);
        let query = query.query();
        let rows = collected(query.column::<&str>("v").expect("Column failed").read());
        assert_eq!(rows, vec!["abc", "de"]);
    }

    /// [`eq`](Adapter::eq) retains only the items bit-identical to the operand.
    #[test]
    fn eq_filter_excludes_non_matching() {
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 3);
        let query = query.query();
        let handle = query.column::<u32>("v").expect("Column failed").eq(20u32);
        assert_eq!(collected(handle.read()), vec![20]);
    }

    /// [`ne`](Adapter::ne) rejects the items bit-identical to the operand.
    #[test]
    fn ne_filter_excludes_matching() {
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 3);
        let query = query.query();
        let handle = query.column::<u32>("v").expect("Column failed").ne(20u32);
        assert_eq!(collected(handle.read()), vec![10, 30]);
    }

    /// A float operand is filtered by **bit pattern**, so set membership accepts an operand type
    /// that is neither [`Eq`] nor [`Hash`] and a bit-identical [`NAN`](f64::NAN) is retained.
    #[test]
    fn float_set_membership_matches_bit_patterns() {
        let items = vec![1.5f64, f64::NAN, 2.5];
        let bytes = items.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::F64, 3);
        let query = query.query();
        let column = query.column::<f64>("v").expect("Column");
        let kept = collected(column.one_of([f64::NAN, 2.5]).read());
        assert_eq!(kept.len(), 2);
        assert!(kept[usize::MIN].is_nan()); // matched by bit pattern, which `PartialEq` cannot do
        assert_eq!(kept[1], 2.5);
    }

    /// The sorted set filters bisect a pre-sorted candidate run, so [`one_of_sorted`][1] and
    /// [`none_of_sorted`][2] agree with the scanning and hashed forms on every ordered operand.
    ///
    /// [1]: Filter::one_of_sorted
    /// [2]: Adapter::none_of_sorted
    #[test]
    fn sorted_set_membership_bisects() {
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 3);
        let query = query.query();
        let kept = query.column::<u32>("v").expect("Column").one_of_sorted([20u32, 30]);
        let none = query.column::<u32>("v").expect("Column").none_of_sorted([20u32]);
        let away = stats(&[8u32, 9]); // statistics straddled by the candidates, so it prunes
        let away = away.query();
        let away = away.column::<u32>("v").expect("Column").one_of_sorted([1u32, 4, 7, 12]);
        assert_eq!(collected(kept.read()), [20, 30]);
        assert_eq!(collected(none.read()), [10, 30]);
        assert!(collected(away.read()).is_empty());
    }

    /// The hashed set filters mirror [`one_of`](Adapter::one_of) and
    /// [`none_of`](Adapter::none_of) exactly, and prune a disjoint buffer the same way.
    #[test]
    fn hashed_set_membership_filters() {
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        let one = query(&bytes, Type::U32, 3);
        let one = one.query();
        let one = one.column::<u32>("v").expect("Column").one_of_set([20u32, 30]);
        assert_eq!(collected(one.read()), [20, 30]);
        let none = query(&bytes, Type::U32, 3);
        let none = none.query();
        let none = none.column::<u32>("v").expect("Column").none_of_set([20u32]);
        assert_eq!(collected(none.read()), [10, 30]);
        let away = stats(&[10u32, 15, 20]); // carries statistics, so a disjoint set prunes it
        let away = away.query();
        let away = away.column::<u32>("v").expect("Column").one_of_set([99u32]);
        assert!(collected(away.read()).is_empty());
    }

    /// Set membership prunes against **each** candidate, not the span between them: a buffer whose
    /// statistics fall in a gap between candidates provably holds no match and is dropped.
    #[test]
    fn set_membership_prunes_between_candidates() {
        let scan = stats(&[8u32, 9]); // statistics span [8, 9]; the candidates straddle it
        let scan = scan.query();
        let scan = scan.column::<u32>("v").expect("Column").one_of([1u32, 4, 7, 12]);
        let hash = stats(&[8u32, 9]);
        let hash = hash.query();
        let hash = hash.column::<u32>("v").expect("Column").one_of_set([1u32, 4, 7, 12]);
        assert!(collected(scan.read()).is_empty());
        assert!(collected(hash.read()).is_empty());
    }

    /// [`into_hash_set`](Adapter::into_hash_set) collects the distinct items;
    /// [`into_hash_map`](Adapter::into_hash_map) maps each to the position of the first
    /// occurrence, with the counter advancing across the duplicates.
    #[test]
    fn unique_and_index_deduplicate_a_column() {
        let bytes = vec![10u32, 20, 10, 30].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 4);
        let query = query.query();
        let column = query.column::<u32>("v").expect("Column");
        let unique = column.into_hash_set().expect("set failed");
        let column = query.column::<u32>("v").expect("Column");
        let index = column.into_hash_map::<u64>().expect("map failed");
        assert_eq!(unique.len(), 3);
        assert!(unique.contains(&10) && unique.contains(&20) && unique.contains(&30));
        assert_eq!(index[&10], u64::MIN); // the earliest occurrence wins
        assert_eq!(index[&20], 1);
        assert_eq!(index[&30], 3); // the duplicate at slot 2 still advances the counter
    }

    /// [`one_of`](Adapter::one_of) retains set members; `none_of` rejects them.
    #[test]
    fn set_membership_filters() {
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        let one = query(&bytes, Type::U32, 3);
        let one = one.query();
        let one = one.column::<u32>("v").expect("Column").one_of([20u32, 30]);
        assert_eq!(collected(one.read()), [20, 30]);
        let none = query(&bytes, Type::U32, 3);
        let none = none.query();
        let none = none.column::<u32>("v").expect("Column").none_of([20u32]);
        assert_eq!(collected(none.read()), [10, 30]);
    }

    /// [`is_some`](filter::IsSome::is_some) retains [`Some`] items;
    /// [`is_none`](filter::IsNone::is_none) retains [`None`] items, delegating validity to the
    /// optional mask.
    #[test]
    fn validity_filters_split_optionals() {
        let bytes = {
            let mut acc = OptBitVec::<u32>::default();
            [Some(1u32), None, Some(3)].into_iter().for_each(|v| acc.push(v));
            acc.serialize().expect("Serialize failed")
        };
        let some = query(&bytes, Type::option(Type::U32), 3);
        let some = some.query();
        let some = some.column::<Option<u32>>("v").expect("Column").is_some();
        assert_eq!(collected(some.read()), vec![1, 3]); // is_some narrows Option<u32> to u32
        let none = query(&bytes, Type::option(Type::U32), 3);
        let none = none.query();
        let none = none.column::<Option<u32>>("v").expect("Column").is_none();
        assert_eq!(collected(none.read()), vec![None]);
    }

    /// An item filter on an optional column tests each [`Some`]; a [`None`] item carries no operand
    /// to test and is **excluded**. Chaining `is_some` is therefore redundant, whereas `is_none`
    /// selects the absent items instead.
    #[test]
    fn item_filter_excludes_none_on_optional() {
        let bytes = {
            let mut acc = OptBitVec::<u32>::default();
            [Some(1u32), None, Some(20)].into_iter().for_each(|v| acc.push(v));
            acc.serialize().expect("Serialize failed")
        };
        let ty = || Type::option(Type::U32);
        let kept = query(&bytes, ty(), 3);
        let kept = kept.query();
        let kept = kept.column::<Option<u32>>("v").expect("Column").eq(20u32);
        assert_eq!(collected(kept.read()), vec![Some(20)]);
        let chained = query(&bytes, ty(), 3);
        let chained = chained.query();
        let handle = chained.column::<Option<u32>>("v").expect("Column");
        let chained = handle.is_some().eq(20u32); // the reverse order no longer compiles
        assert_eq!(collected(chained.read()), vec![20]); // narrowed past the option
        let absent = query(&bytes, ty(), 3);
        let absent = absent.query();
        let absent = absent.column::<Option<u32>>("v").expect("Column").is_none();
        assert_eq!(collected(absent.read()), vec![None]);
    }

    /// A filter operand of the wrong type is rejected before any file IO.
    #[test]
    fn eq_type_mismatch_errors() {
        let bytes = vec![1u32].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 1);
        let query = query.query();
        query.column::<bool>("v").expect_err("Type mismatch accepted");
    }

    /// An [`eq`](filter::Filter::eq) disjoint from the buffer statistics prunes it; the handle
    /// empties and the stream is empty.
    #[test]
    fn eq_prunes_disjoint_column() {
        let query = stats(&[10u32, 15, 20]);
        let query = query.query();
        let handle = query.column::<u32>("v").expect("Column").eq(100u32);
        assert!(collected(handle.read()).is_empty());
    }

    /// Extracting an unknown column name is rejected with [`Error::Column`].
    #[test]
    fn column_unknown_name_errors() {
        let bytes = vec![1u32].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 1);
        let query = query.query();
        let error = query.column::<u32>("missing").expect_err("Unknown column accepted");
        assert!(matches!(error, Error::Column { .. }));
    }

    /// A composite read over a populated schema rebuilds one item per committed slot.
    #[test]
    fn read_rebuilds_composite_items() {
        let fixture = stats(&[10u32, 20, 30]);
        let query = fixture.query();
        let rows = query.iter::<Composed>().expect("composite read rejected");
        let items: Vec<u32> = rows.map(|row| row.expect("item failed").v).collect();
        assert_eq!(items, [10, 20, 30]);
    }

    /// A composite read over an empty column map is rejected with [`Error::Column`].
    ///
    /// Every composite names at least one column – `#[derive(Read)]` rejects a type with no
    /// fields – so an empty map cannot satisfy one, and reporting the missing column beats
    /// returning an empty stream that hides the mismatch.
    #[test]
    fn read_empty_column_map_errors() {
        let fixture = Fixture::new(&[], []);
        let query = fixture.query();
        let error = query.iter::<Composed>().err(); // opaque Ok, so no expect_err
        assert!(matches!(error, Some(Error::Column { .. })));
    }

    /// A composite read over a populated schema that lacks a named field is rejected with
    /// [`Error::Column`]; the column is genuinely absent rather than empty.
    #[test]
    fn read_absent_column_errors() {
        let fixture = Fixture::new(&[], [column("w", Type::U32, [])]);
        let query = fixture.query();
        let error = query.iter::<Composed>().err(); // opaque Ok, so no expect_err
        assert!(matches!(error, Some(Error::Column { .. })));
    }

    /// Two handles filter independently: filtering `a` leaves `b` untouched.
    #[test]
    fn handles_filter_per_column() {
        let query = pair(&[10, 20, 30], &[1, 2, 3]);
        let query = query.query();
        let a = query.column::<u32>("a").expect("Column a").range(15u32..25);
        assert_eq!(collected(a.read()), [20]);
        assert_eq!(
            collected(query.column::<u32>("b").expect("Column b").read()),
            [1, 2, 3]
        );
    }

    /// [`and`](Join::and) intersects the tagged buffer lists of two handles; an item
    /// filter that prunes one side prunes the sibling on sync.
    #[test]
    fn join_syncs_buffers() {
        // Column `a` carries restrictive statistics [10, 30]; `b` spans the full range. An
        // `eq(100)` on `a` is provably disjoint, so its sole buffer is pruned before the join.
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        // Column `a` carries real statistics resolved from the map; `b` carries none.
        let stats = detailed(bytes.len(), 3, u64::MIN, 2);
        let basic = span(usize::MIN, bytes.len(), 3);
        let left = column("a", Type::U32, [stats]);
        let right = column("b", Type::U32, [basic]);
        let query = Fixture::new(&bytes, [left, right]);
        let query = query.query();
        let a = query.column::<u32>("a").expect("Column a").eq(100u32); // prunes a
        let b = query.column::<u32>("b").expect("Column b");
        let join = a.and(b).expect("join failed");
        let mut mask = Src::mask(&join);
        mask::Resolve::resolve(join, &mut mask).expect("resolve failed");
        assert!(mask.not_any()); // leg a proves the buffer empty, so the intersection clears it
    }

    /// [`or`](Join::or) offers `b` the buffers `a` cleared and unites both masks, without
    /// resurrecting a buffer an enclosing node cleared first.
    ///
    /// Each side of the union contributes one buffer, so dropping either half is visible: column
    /// `b` supplies the second buffer and column `c` the third. Column `c` retains every buffer it
    /// is offered, so the first buffer survives only if the enclosing [`Conjunct`] fails to
    /// withhold it.
    #[test]
    fn disjunct_unites_within_the_offered_buffers() {
        let fixture = segmented([[1, 2, 3], [4, 5, 6], [7, 8, 9]]);
        let query = fixture.query();
        let a = query.column::<u32>("a").expect("Column a").one_of([2u32, 3]); // clears buffer 0
        let b = query.column::<u32>("b").expect("Column b").eq(5u32); // keeps buffer 1 alone
        let c = query.column::<u32>("c").expect("Column c"); // keeps whatever it is offered
        let tree = a.and(b.or(c).expect("or failed")).expect("and failed");
        let mut mask = Src::mask(&tree);
        mask::Resolve::resolve(tree, &mut mask).expect("resolve failed");
        assert_eq!(mask.count_ones(), 2); // buffer 1 from column b, buffer 2 from column c
        assert!(mask[usize::MIN].not()); // never buffer 0, which column a excluded before the union
    }

    /// An [`and`](Join::and) across handles from different queries is rejected.
    #[test]
    fn cross_query_join_errors() {
        let one = pair(&[1, 2], &[3, 4]);
        let one = one.query();
        let two = pair(&[1, 2], &[3, 4]);
        let two = two.query();
        let a = one.column::<u32>("a").expect("Column a");
        let b = two.column::<u32>("b").expect("Column b");
        let joined = a.and(b).err(); // bound so the matcher does not nest a call (rule 13)
        assert!(matches!(joined, Some(Error::Join)));
    }

    /// A [`skip`](Adapter::skip) residual shifts the stream origin, so a following
    /// [`take`](Adapter::take) must reach that far past it — across a buffer boundary if
    /// the window demands it, rather than measuring from the trimmed buffer map alone.
    #[test]
    fn skip_take_window_spans_buffers() {
        let query = spread(&[10, 20, 30, 40], &[50, 60, 70, 80]);
        let query = query.query();
        let whole = query.column::<u32>("v").expect("Column").skip(0).take(8);
        let across = query.column::<u32>("v").expect("Column").skip(2).take(3);
        let inside = query.column::<u32>("v").expect("Column").skip(5).take(2);
        assert_eq!(collected(whole.read()), [10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(collected(across.read()), [30, 40, 50]); // slots 2, 3, 4 cross the boundary
        assert_eq!(collected(inside.read()), [60, 70]); // slots 5, 6 sit inside the second
    }

    /// [`skip`](Adapter::skip) and [`take`](Adapter::take) window a handle by
    /// positional slot without deserializing outside the window; [`nth`][1] extracts one slot.
    ///
    /// [1]: Adapter::nth
    #[test]
    fn column_skip_take_and_nth() {
        let bytes = vec![10u32, 20, 30, 40].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 4);
        let query = query.query();
        let window = query.column::<u32>("v").expect("Column").skip(1).take(2);
        assert_eq!(collected(window.read()), [20, 30]);
        let item = query.column::<u32>("v").expect("Column").nth(3).expect("nth failed");
        assert_eq!(item, Some(40));
    }

    /// Two columns extracted from one query window independently to the identical slot range.
    #[test]
    fn columns_window_in_lockstep() {
        let query = pair(&[10, 20, 30], &[1, 2, 3]);
        let query = query.query();
        let a = query.column::<u32>("a").expect("Column a").skip(1).take(2);
        let b = query.column::<u32>("b").expect("Column b").skip(1).take(2);
        assert_eq!(collected(a.read()), [20, 30]);
        assert_eq!(collected(b.read()), [2, 3]);
    }

    /// [`skip`](Adapter::skip) clears the buffers wholly covered by the request and leaves the
    /// residual to the stream; a skip beyond the committed items empties the handle.
    #[test]
    fn skip_trims_whole_buffers_and_residual() {
        let fixture = spread(&[10, 20, 30, 40], &[50, 60, 70, 80]);
        let query = fixture.query();
        let across = query.column::<u32>("v").expect("Column").skip(5);
        let exact = query.column::<u32>("v").expect("Column").skip(4);
        let past = query.column::<u32>("v").expect("Column").skip(9);
        assert_eq!(collected(across.read()), [60, 70, 80]); // one buffer cleared, residual 1
        assert_eq!(collected(exact.read()), [50, 60, 70, 80]); // a buffer boundary
        assert!(collected(past.read()).is_empty()); // a skip beyond the items yields nothing
    }

    /// [`take`](Adapter::take) clears the buffers beginning at or beyond the window end and leaves
    /// the residual to the stream; a take beyond the committed items keeps every buffer.
    #[test]
    fn take_keeps_whole_buffers_and_residual() {
        let fixture = spread(&[10, 20, 30, 40], &[50, 60, 70, 80]);
        let query = fixture.query();
        let kept = query.column::<u32>("v").expect("Column").take(5);
        let cut = query.column::<u32>("v").expect("Column").take(4);
        let whole = query.column::<u32>("v").expect("Column").take(9);
        assert_eq!(collected(kept.read()), [10, 20, 30, 40, 50]); // reaches the second buffer
        assert_eq!(collected(cut.read()), [10, 20, 30, 40]); // ends the first buffer exactly
        assert_eq!(collected(whole.read()), [10, 20, 30, 40, 50, 60, 70, 80]);
    }

    /// A filter enclosed by [`skip`](Adapter::skip) clears buffers before the skip descends, so
    /// the skip must pass over an already-excluded buffer rather than stopping at it.
    #[test]
    fn skip_descends_past_an_already_excluded_buffer() {
        let fixture = mixed(10, 4, &[50, 60, 70, 80]);
        let query = fixture.query();
        let handle = query.column::<u32>("v").expect("Column").range(50u32..).skip(2);
        assert_eq!(collected(handle.read()), [70, 80]); // never [50, 60, 70, 80]
    }

    /// A [`skip`](Adapter::skip) residual is anchored to the buffer holding the first kept item,
    /// so a later filter clearing that buffer drops the residual with it.
    ///
    /// Without the anchor the offset lands on the next buffer instead, silently losing items that
    /// the window never covered.
    #[test]
    fn skip_residual_dies_with_a_pruned_anchor() {
        let fixture = mixed(10, 4, &[50, 60, 70, 80]);
        let query = fixture.query();
        let handle = query.column::<u32>("v").expect("Column").skip(2).range(50u32..);
        assert_eq!(collected(handle.read()), [50, 60, 70, 80]); // never [70, 80]
    }

    /// A [`take`](Adapter::take) window is recounted against the settled mask, so a later
    /// filter clearing a buffer inside the window shrinks it rather than sliding it past the end.
    #[test]
    fn take_window_shrinks_under_a_later_prune() {
        let fixture = mixed(10, 4, &[50, 60, 70, 80]);
        let query = fixture.query();
        let handle = query.column::<u32>("v").expect("Column").take(6).range(50u32..);
        assert_eq!(collected(handle.read()), [50, 60]); // never [50, 60, 70, 80]
    }

    /// A semi-join keeps the items whose key appears in a column of a **separate** query; an
    /// anti-join keeps exactly the complement.
    ///
    /// Each fixture owns a separate memory map, which is the cross-schema case the combination
    /// operators cannot express.
    #[test]
    fn semi_and_anti_join_split_a_column_by_another_query() {
        let bytes = vec![10u32, 20, 30, 40].serialize().expect("Serialize failed");
        let left = query(&bytes, Type::U32, 4);
        let left = left.query();
        let bytes = vec![20u32, 40].serialize().expect("Serialize failed");
        let right = query(&bytes, Type::U32, 2);
        let right = right.query();
        let kept = left.column::<u32>("v").expect("Column");
        let kept = kept.semi_join(right.column::<u32>("v").expect("Column"));
        let away = left.column::<u32>("v").expect("Column");
        let away = away.anti_join(right.column::<u32>("v").expect("Column"));
        assert_eq!(collected(kept.read()), [20, 40]);
        assert_eq!(collected(away.read()), [10, 30]);
    }

    /// A semi-join clears a compact buffer whose repeated item matches no key, before any file IO.
    #[test]
    fn semi_join_prunes_a_compact_buffer_holding_no_key() {
        let bytes = vec![7u32].serialize().expect("Serialize failed");
        let left = compact(&bytes, Type::U32, 3);
        let left = left.query();
        let bytes = vec![99u32].serialize().expect("Serialize failed");
        let right = query(&bytes, Type::U32, 1);
        let right = right.query();
        let away = left.column::<u32>("v").expect("Column");
        let away = away.semi_join(right.column::<u32>("v").expect("Column"));
        assert!(collected(away.read()).is_empty()); // 7 matches no key, so the buffer is cleared
    }
}
