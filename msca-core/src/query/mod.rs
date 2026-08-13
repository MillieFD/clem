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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::{self, Display};
use std::hash::Hash;
use std::marker::PhantomData;
use std::num::TryFromIntError;
use std::ops::{Deref, RangeBounds};
use std::sync::Arc;

use bitvec::boxed::BitBox;
use bitvec::vec::BitVec;
use funty::Unsigned;
use memmap2::Mmap;
use xxhash_rust::xxh3::Xxh3Builder;

use crate::io::{self, Deserialize, Deserializer};
use crate::manifest::{self, Buffer};
use crate::read::{Composite, Evaluate, IsOption, Outcome, Read, Reader, Resolve, Unfiltered};
use crate::schema::{self, Schema, Type, Unfolder, number};

/* ------------------------------------------------------------------------------ Public Exports */

/// A composable query interface to [read](Read) data from any [msca](crate) file; initialised from
/// [`Dataset::query`][1] and executed lazily when [`iter`](Self::iter) is polled.
///
/// [`Query`] also provides a [column](Adapter) factory for the specified [`Schema`].
///
/// Refer to the [module-level documentation](self) for implementation details.
///
/// [1]: crate::Dataset::query
#[derive(Clone, Debug)]
pub struct Query<'m> {
    /// Read-only [memory map](Mmap) backed by the immutable segment region.
    ///
    /// Refer to the [safety documentation](io::File::mmap) for details.
    pub(crate) mmap: Arc<Mmap>,
    /// On-disk [`Column`][1] descriptors borrowed from the [manifest] and keyed by name.
    ///
    /// [`BTreeMap`] guarantees a deterministic column order for consistent [serialisation][2] and
    /// [`Schema`] comparison.
    ///
    /// [1]: manifest::Column
    /// [2]: crate::accumulate::Serialize
    // NOTE: borrowed entries are zero-copy; owned map can remove entries without breaking manifest
    pub(crate) columns: BTreeMap<&'m str, &'m manifest::Column>,
}

impl<'m> Query<'m> {
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
    pub fn indexed<'q, I, N>(&'q self) -> Result<HashMap<I, N, Xxh3Builder>, Error>
    where
        N: Unsigned,
        I: Unfiltered<'q> + Eq + Hash + 'q,
    {
        let iter = self.iter::<I>()?;
        Self::intern(iter)
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
    /// - [`Error::Number`] if an index overflows `N`.
    /// - [`Error::Io`] if a deserialization failure occurs.
    ///
    /// Refer to [`Query::indexed`] and [`Adapter::indexed`] for the public entry points.
    fn intern<I, N, S>(items: S) -> Result<HashMap<I, N, Xxh3Builder>, Error>
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
    /// - [`Error::Column`] if `name` is not found in the query [`BTreeMap`].
    /// - [`Error::Type`] if the requested `Type` does not match the on-disk column type.
    pub fn column<'q, I>(&'q self, name: &str) -> Result<Column<'q, I>, Error>
    where
        I: Read + Clone + 'q,
        I::Src<'q>: Deserialize<'q, Ok = I::Src<'q>> + Reader<'q, I>,
        Schema: Unfolder<I>,
    {
        if let Some(entry) = self.columns.get(name) {
            let buffers = &entry.exact::<I>()?.buffers;
            // SAFETY: on-disk column type verified against requested I via manifest::Column::exact
            let column = Src { query: self, buffers }.into();
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
    pub fn nth<'q, I>(&'q self, n: usize) -> Result<Option<I>, Error>
    where
        I: Unfiltered<'q> + 'q,
    {
        I::nth(self, n)?.resolve().next().transpose().map_err(Error::from)
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
    /// - [`Error::Column`] if `name` is not found in the query [`BTreeMap`].
    /// - [`Error::Type`] if the requested type is incompatible with the on-disk column type.
    /// - [`Error::Io`] if a per-buffer source cannot be constructed from the memory map.
    ///
    /// Refer to [`Query::iter`] for a resolved alternative that automatically re-polls the iterator
    /// to yield only [included](Outcome::Include) items.
    ///
    /// [1]: Deserialize::deserialize
    pub fn read<'q, I>(&'q self, name: &str) -> Result<impl Iterator<Item = Outcome<I>>, Error>
    where
        I: Read + Clone + 'q,
        I::Src<'q>: Deserialize<'q, Ok = I::Src<'q>> + Reader<'q, I>,
        Schema: Unfolder<I>,
    {
        let buffers = self
            .columns
            .get(name)
            .ok_or_else(|| Error::Column { name: name.into() })?
            .exact::<I>()?
            .buffers
            .iter();
        let items = iter::Src::new(buffers, &self.mmap).iter()?.map(Outcome::from);
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
    pub fn iter<'q, I>(&'q self) -> Result<impl Iterator<Item = Result<I, io::Error>> + 'q, Error>
    where
        I: Unfiltered<'q> + 'q,
    {
        let iter = I::unfiltered(self)?.resolve();
        Ok(iter)
    }

    /// Returns the total number of on-disk items for this [`Schema`] across every segment; the sum
    /// of [`Buffer`](Buffer)`::`[`count`](Buffer::count) for one [`Column`](manifest::Column).
    pub fn count(&self) -> u64 {
        // NOTE: copied fn dereferences &&Column → &Column (no performance cost).
        self.columns.values().next().copied().map(manifest::Column::count).unwrap_or_default()
    }
}

impl<'m> PartialEq for Query<'m> {
    /// Returns `true` if two queries:
    ///
    /// 1. Read the same memory map [`Arc`] pointer.
    /// 2. Expose the same [`Schema`].
    ///
    /// Read the [trait documentation](PartialEq) for more details.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.mmap, &other.mmap) && self.columns == other.columns
    }
}

impl<'m> Eq for Query<'m> {}

/// An immutable **byte source** for one [`Column`] and all subsequent [adapters](Adapter).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Src<'q> {
    /// An immutable reference to the parent [`Query`].
    query: &'q Query<'q>,
    /// [`Buffer`] descriptors for the [`Column`][1] across all segments in [`Sector`][2] order.
    ///
    /// [1]: manifest::Column
    /// [2]: io::Sector
    // NOTE: sector offset increases monotonically → sector order matches on-disk segment order
    buffers: &'q BTreeSet<Buffer>,
}

impl<'q> Src<'q> {
    /// Returns a new [mask](BitBox) that includes every [`Buffer`] for this column.
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
    pub(crate) fn mask(&self) -> BitBox {
        let n = self.buffers.len();
        BitVec::repeat(true, n).into_boxed_bitslice()
    }

    /// Returns the logical number of items recorded in each [`Buffer`].
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Number`] if any recorded count overflows [`usize`].
    pub(crate) fn counts(&self, mask: &BitBox) -> impl Iterator<Item = Result<usize, Error>> {
        self.buffers.iter().zip(mask.iter().by_vals()).map(|e| match e.1 {
            true => e.0.count().try_into().map_err(Error::from),
            false => Ok(usize::MIN),
        })
    }

    /// Returns **only** the [`Buffer`] descriptors included by the [mask](BitBox) in [`Sector`][1]
    /// order.
    ///
    /// [1]: io::Sector
    // NOTE: owned slice can outlive the mask; Box prevents resize allocations at the type level
    pub(crate) fn retained(&self, mask: &BitBox) -> Box<[&'q Buffer]> {
        mask.iter().by_vals().zip(self.buffers).filter(|b| b.0).map(|b| b.1).collect()
    }

    /// An iterator method that applies a fallible [test](FnMut) to each [`Buffer`] descriptor:
    ///
    /// - Skips any buffers that are already excluded by the [mask](BitBox).
    /// - Retains all buffers for which `test` returns `true`.
    /// - Excludes any buffers for which `test` returns `false`.
    ///
    /// Returns the number of included buffers. Refer to [`Src::mask`] for more details.
    ///
    /// ### Errors
    ///
    /// Forwards [`Error::Io`] from the fallible `test` function.
    pub(crate) fn try_exclude<F>(&self, mask: &mut BitBox, mut test: F) -> Result<usize, io::Error>
    where
        F: FnMut(&Buffer, &'q Mmap) -> Result<bool, io::Error>,
    {
        mask.iter_mut().zip(self.buffers).try_fold(usize::MIN, |n, (bit, buf)| {
            if *bit {
                match test(buf, &self.query.mmap)? {
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
pub struct Column<'q, I> {
    /// The on-disk source this column reads from.
    src: Src<'q>,
    /// Zero-sized type-state for the requested [`item`](I) type.
    item: PhantomData<I>,
}

impl<'q, I> From<Src<'q>> for Column<'q, I> {
    fn from(src: Src<'q>) -> Self {
        Self { src, item: PhantomData }
    }
}

impl<'q, I> Deref for Column<'q, I> {
    type Target = Src<'q>;

    fn deref(&self) -> &Src<'q> {
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
pub struct Filter<'q, S, F, I>
where
    S: Source<'q>,
    F: Fn(&I) -> bool,
{
    /// The wrapped data source which yields [deserialized](Deserialize) items for the
    /// [`filter`](filter::Filter::filter) closure.
    source: S,
    /// The [`filter`](filter::Filter::filter) used to assess each [deserialized](Deserialize) item.
    filter: F,
    /// Zero-sized **marker** carrying the item type and [`Query`] lifetime.
    phantom: PhantomData<&'q I>,
}

impl<'q, S, F, I> Deref for Filter<'q, S, F, I>
where
    S: Source<'q>,
    F: Fn(&I) -> bool,
{
    type Target = Src<'q>;

    fn deref(&self) -> &Src<'q> {
        &self.source
    }
}

/// A [column][1] [adapter](Adapter) that retains **only** items within the specified [range][2].
///
/// [1]: manifest::Column
/// [2]: RangeBounds
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
// NOTE: this struct is used for both "mask" and "iter" adapter chains
pub struct Range<'q, S, B, I>
where
    S: Source<'q>,
    B: RangeBounds<I>,
{
    /// The wrapped data [`Source`] which yields [deserialized](Deserialize) items.
    source: S,
    /// The [range](RangeBounds) of items to retain.
    bounds: B,
    /// Zero-sized **marker** carrying the item type and [`Query`] lifetime.
    phantom: PhantomData<&'q I>,
}

impl<'q, S, B, I> Deref for Range<'q, S, B, I>
where
    S: Source<'q>,
    B: RangeBounds<I>,
{
    type Target = Src<'q>;

    fn deref(&self) -> &Src<'q> {
        &self.source
    }
}

/// A [column][1] [adapter](Adapter) that retains **only** items [matching](BitMatch) one target.
///
/// [1]: manifest::Column
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
// NOTE: this struct is used for both "mask" and "iter" adapter chains
pub struct BitMatch<'q, S, I>
where
    S: Source<'q>,
    I: schema::BitMatch,
{
    /// The wrapped data [`Source`] which yields [deserialized](Deserialize) items.
    source: S,
    /// The target against which each [deserialized](Deserialize) item is [assessed](BitMatch).
    item: I,
    /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
    phantom: PhantomData<&'q Mmap>,
}

impl<'q, S, I> Deref for BitMatch<'q, S, I>
where
    S: Source<'q>,
    F: filter::BoundedFilter<I>,
{
    /// The wrapped data [`Source`] which yields [deserialized](Deserialize) items.
    source: S,
    /// The [bounded filter](filter::BoundedFilter) used to assess each [`Buffer`] and
    /// [deserialized](Deserialize) item.
    filter: F,
    /// Zero-sized **marker** carrying the operand type and [`Query`] lifetime.
    item: PhantomData<&'q I>,
}

impl<'q, S, I> Deref for OneOf<'q, S, I>
where
    S: Source<'q>,
    I: schema::BitMatch,
{
    type Target = Src<'q>;

    fn deref(&self) -> &Src<'q> {
        &self.source
    }
}

/// A [column](manifest::Column) [adapter](Adapter) that retains **only** items [matching](BitMatch)
/// at least one candidate from an ordered [collection](std::collections).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
// NOTE: this struct is used for both "mask" and "iter" adapter chains
pub struct OneOfSorted<'q, S, I>
where
    S: Source<'q>,
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
    phantom: PhantomData<&'q Mmap>,
}

impl<'q, S, I> Deref for OneOfSorted<'q, S, I>
where
    S: Source<'q>,
    I: Ord,
{
    type Target = Src<'q>;

    fn deref(&self) -> &Src<'q> {
        &self.source
    }
}

/// A [column](manifest::Column) [adapter](Adapter) that retains **only** items [present](BitMatch)
/// in the specified [hashed](std::hash::Hasher) candidate [set](HashSet).
///
/// [1]: manifest::Column
#[derive(Clone, Debug)]
// NOTE: this struct is used for both "mask" and "iter" adapter chains
pub struct OneOfSet<'q, S, I>
where
    S: Source<'q>,
    I: Eq + Hash,
{
    /// The wrapped data [`Source`] which yields [deserialized](Deserialize) items.
    source: S,
    /// The [hashed][1] candidate [set](HashSet) probed for each [deserialized](Deserialize) item.
    ///
    /// [1]: std::hash::Hasher
    items: HashSet<I, Xxh3Builder>,
    /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
    phantom: PhantomData<&'q Mmap>,
}

impl<'q, S, I> Deref for OneOfSet<'q, S, I>
where
    S: Source<'q>,
    I: Eq + Hash,
{
    type Target = Src<'q>;

    fn deref(&self) -> &Src<'q> {
        &self.source
    }
}

/// A [column](manifest::Column) [adapter](Adapter) that discards [`None`] items.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct IsSome<'q, S, I>
where
    S: Source<'q>,
    I: Read,
{
    /// The wrapped data [`Source`] which yields [deserialized](Deserialize) items.
    source: S,
    /// Zero-sized **marker** carrying the flattened [`Some`] type and [`Query`] lifetime.
    phantom: PhantomData<&'q I>,
}

impl<'q, S, I> Deref for IsSome<'q, S, I>
where
    S: Source<'q>,
    I: Read,
{
    type Target = Src<'q>;

    fn deref(&self) -> &Src<'q> {
        &self.source
    }
}

/// A [column](manifest::Column) [adapter](Adapter) that retains only [`None`] items.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct IsNone<'q, S>
where
    S: Source<'q>,
{
    /// The wrapped data [`Source`] which yields [deserialized](Deserialize) items.
    source: S,
    /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
    phantom: PhantomData<&'q Mmap>,
}

impl<'q, S> Deref for IsNone<'q, S>
where
    S: Source<'q>,
{
    type Target = Src<'q>;

    fn deref(&self) -> &Src<'q> {
        &self.source
    }
}

/// A [column](manifest::Column) [adapter](Adapter) that skips the first `n` items.
///
/// This adapter is initialised via [`Adapter::skip`] and excludes any [buffers](Buffer) that are
/// provably disjoint from the requested result set.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Skip<'q, S>
where
    S: Adapter<'q>,
{
    /// The wrapped [`Adapter`] which yields [deserialized](Deserialize) items.
    source: S,
    /// The number of items to [`skip`](Adapter::skip).
    skip: usize,
    /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
    phantom: PhantomData<&'q Mmap>,
}

impl<'q, S> Deref for Skip<'q, S>
where
    S: Adapter<'q>,
{
    type Target = Src<'q>;

    fn deref(&self) -> &Src<'q> {
        &self.source
    }
}

/// A [column](manifest::Column) [adapter](Adapter) that reads at most `n` items.
///
/// This adapter is initialised via [`Adapter::take`] and excludes any [buffers](Buffer) that are
/// provably disjoint from the requested result set.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Take<'q, S>
where
    S: Adapter<'q>,
{
    /// The wrapped [`Adapter`] which yields [deserialized](Deserialize) items.
    source: S,
    /// The number of items to [`take`](Adapter::take).
    take: usize,
    /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
    phantom: PhantomData<&'q Mmap>,
}

impl<'q, S> Deref for Take<'q, S>
where
    S: Adapter<'q>,
{
    type Target = Src<'q>;

    fn deref(&self) -> &Src<'q> {
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

/// A [column][1] [adapter](Adapter) retaining only items from `S` that are also present in `K`.
///
/// [1]: manifest::Column
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct SemiJoin<'q, S, K>
where
    S: Adapter<'q>,
    K: Adapter<'q>,
{
    /// The data [`Source`] that yields [deserialized](Deserialize) items restricted by `K`.
    source: S,
    /// The data [`Source`] that yields [deserialized](Deserialize) items to include from `S`.
    keys: K,
    /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
    phantom: PhantomData<&'q Mmap>,
}

impl<'q, S, K> Deref for SemiJoin<'q, S, K>
where
    S: Adapter<'q>,
    K: Adapter<'q>,
{
    type Target = Src<'q>;

    fn deref(&self) -> &Src<'q> {
        &self.source
    }
}

/// A [column][1] [adapter](Adapter) retaining only items from `S` that are **not** present in `K`.
///
/// [1]: manifest::Column
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct AntiJoin<'q, S, K>
where
    S: Adapter<'q>,
    K: Adapter<'q>,
{
    /// The data [`Source`] that yields [deserialized](Deserialize) items restricted by `K`.
    source: S,
    /// The data [`Source`] that yields [deserialized](Deserialize) items to exclude from `S`.
    keys: K,
    /// Zero-sized **marker** carrying the [`Mmap`] lifetime read by the [`Source`].
    phantom: PhantomData<&'q Mmap>,
}

impl<'q, S, K> Deref for AntiJoin<'q, S, K>
where
    S: Adapter<'q>,
    K: Adapter<'q>,
{
    type Target = Src<'q>;

    fn deref(&self) -> &Src<'q> {
        &self.source
    }
}

/* --------------------------------------------------------------------- Source Trait Definition */

/// A [data source](Src) or [adapter chain](Adapter) over one [`Column`](manifest::Column) that
/// yields the specified [`Item`](Self::Item) type.
///
/// ### Guidance
///
/// Each [`Filter`] is applied in the order of declaration. Filters are lazy and short-circuiting:
/// Downstream filters **never** assess [buffers](Buffer) and [deserialized](Deserialize) items that
/// are already excluded by upstream filters. Users are therefore advised to think carefully about
/// the filter order; declare more restrictive filters early to reduce the result set quickly and
/// minimise work for subsequent filters.
///
/// ### Implementation
///
/// This trait carries a `'q` lifetime from the parent [`Query`] to ensure no item outlives the file
/// from which it was [deserialized](Deserialize). This design enables zero-copy reads. [`Clone`]
/// the item to outlive `'q`.
///
/// ##### Mask Adapters Before IO
///
/// Every chain begins from a [data source](Src) that borrows the candidate [`Buffer`] set. Each
/// filter wraps the chain in a lazy adapter that captures the necessary state to assess whole
/// buffers. Every adapter is [monomorphized][1] against the concrete item type. No [`IO`](io) is
/// executed until a terminal method is called e.g. [`read`][2] or [`iter`][3].
///
/// ##### Item Adapters During IO
///
/// The buffer filter chain is [resolved][4] into an item filter [`Iterator`] chain of the same
/// shape. The innermost [data source](iter::Src) deserializes items from **only** the surviving
/// buffers. Enclosing adapters test the item and yield an [`Outcome`], immediately short-circuiting
/// once the item is [excluded](Outcome::exclude).
///
/// This trait is implemented by single-column **mask** and **item** adapters in both phases of the
/// query lifecycle. This trait is **not** implemented by [combined](Combine) adapters which join
/// multiple columns.
///
/// [1]: https://rustc-dev-guide.rust-lang.org/backend/monomorph.html
/// [2]: Adapter::read
/// [3]: Adapter::iter
/// [4]: mask::Resolve::resolve
pub trait Source<'q>: Deref<Target = Src<'q>>
where
    <Self::Item as Read>::Src<'q>: Deserialize<'q, Ok = <Self::Item as Read>::Src<'q>>,
    <Self::Item as Read>::Src<'q>: Reader<'q, Self::Item>,
{
    /// The [deserialized](Deserialize) item type [read](Read) by the chain.
    type Item: Read + 'q;
}

/* ----------------------------------------------------------------- Source Trait Implementation */

impl<'q, I> Source<'q> for Column<'q, I>
where
    I: Read + Clone + 'q,
    I::Src<'q>: Deserialize<'q, Ok = I::Src<'q>> + Reader<'q, I>,
{
    type Item = I;
}

impl<'q, S, F, I> Source<'q> for Filter<'q, S, F, I>
where
    S: Source<'q>,
    F: Fn(&I) -> bool,
{
    type Item = S::Item;
}

impl<'q, S, B, I> Source<'q> for Range<'q, S, B, I>
where
    S: Source<'q>,
    B: RangeBounds<I>,
{
    type Item = S::Item;
}

impl<'q, S, I> Source<'q> for BitMatch<'q, S, I>
where
    S: Source<'q>,
    I: schema::BitMatch,
{
    type Item = S::Item;
}

impl<'q, S, I> Source<'q> for OneOf<'q, S, I>
where
    S: Source<'q>,
    I: schema::BitMatch,
{
    type Item = S::Item;
}

impl<'q, S, I> Source<'q> for OneOfSorted<'q, S, I>
where
    S: Source<'q>,
    I: Ord,
{
    type Item = S::Item;
}

impl<'q, S, I> Source<'q> for OneOfSet<'q, S, I>
where
    S: Source<'q>,
    I: Eq + Hash,
{
    type Item = S::Item;
}
impl<'q, S, I> Source<'q> for IsSome<'q, S, I>
where
    S: Source<'q>,
    I: Read + Clone + 'q,
    I::Src<'q>: Deserialize<'q, Ok = I::Src<'q>> + Reader<'q, I>,
{
    type Item = I;
}

impl<'q, S> Source<'q> for IsNone<'q, S>
where
    S: Source<'q>,
{
    type Item = S::Item;
}

impl<'q, S> Source<'q> for Skip<'q, S>
where
    S: Adapter<'q>,
{
    type Item = S::Item;
}

impl<'q, S> Source<'q> for Take<'q, S>
where
    S: Adapter<'q>,
{
    type Item = S::Item;
}

impl<'q, S, K> Source<'q> for SemiJoin<'q, S, K>
where
    S: Adapter<'q>,
    K: Adapter<'q>,
{
    type Item = S::Item;
}

impl<'q, S, K> Source<'q> for AntiJoin<'q, S, K>
where
    S: Adapter<'q>,
    K: Adapter<'q>,
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
pub(crate) trait Exclude<'q>: Source<'q> + Sized {
    /// An [excluder](Exclude) method that applies a fallible [filter](Fn) to each [`Buffer`]
    /// descriptor without [detailed](Buffer::Detailed) variant exclusion using statistics.
    ///
    /// ### Guidance
    ///
    /// Use [`Exclude::with_min_max`] for filters that **can** assess detailed candidates.
    ///
    /// ### Errors
    ///
    /// - Returns [`Error::Io`] if a [compact][1] item cannot be read from the [memory map](Mmap).
    /// - Forwards [`Error::Io`] from the fallible `test` function.
    ///
    /// Refer to the [`Src::try_exclude`] documentation for the underlying iteration method.
    ///
    /// [1]: Buffer::Compact
    fn with_item<I, F>(&self, mask: &mut BitBox, filter: F) -> Result<usize, Error>
    where
        Self::Item: Evaluate<I>,
        F: Fn(&I) -> bool,
    {
        self.try_exclude(mask, |buf, mmap| {
            if let Buffer::Compact { .. } = buf {
                let item = buf.item::<Self::Item>(mmap)?;
                let kept = item.evaluate(&filter);
                Ok(kept)
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
    /// - Returns [`Error::Io`] if a [compact][1] item cannot be read from the [memory map](Mmap).
    /// - Forwards [`Error::Io`] from the fallible `test` function.
    ///
    /// Refer to the [`Src::try_exclude`] documentation for the underlying iteration method.
    ///
    /// [1]: Buffer::Compact
    fn with_min_max<I, F, R>(&self, mask: &mut BitBox, item: F, range: R) -> Result<usize, Error>
    where
        Self::Item: Evaluate<I>,
        I: for<'de> Deserialize<'de, Ok = I> + 'q,
        F: Fn(&I) -> bool,
        R: Fn(&I, &I) -> bool,
    {
        self.try_exclude(mask, |buf, mmap| {
            if let Buffer::Detailed { min, max, .. } = buf {
                let min: I = min.slice(mmap)?.deserialize_into()?;
                let max: I = max.slice(mmap)?.deserialize_into()?;
                let keep = f_range(&min, &max);
                Ok(keep)
            } else if let Buffer::Compact { .. } = buf {
                let keep = buf.item::<Self::Item>(mmap)?.evaluate(&f_item);
                Ok(keep)
            } else {
                Ok(true)
            }
        })
    }
}

/* ---------------------------------------------------------------- Exclude Trait Implementation */

impl<'q, S> Exclude<'q> for S where S: Source<'q> + Sized {}

/* ------------------------------------------------------------------------ Buffer Filter Module */

pub mod mask {

    use std::ops::Deref;

    use bitvec::boxed::BitBox;

    use super::{Error, Src};
    use crate::io;

    /* ---------------------------------------------------------------- Resolve Trait Definition */

    /// A [buffer][1] [filter][2] chain that reduces the candidate buffer [mask](BitBox) before
    /// [resolving](Resolve::resolve) into an [item filter chain][3] of the same shape.
    ///
    /// ### Lifetime
    ///
    /// This trait carries a `'q` lifetime from the parent [`Query`](super::Query) to ensure that no
    /// item outlives the file from which it was [deserialized](Deserialize). This design enables
    /// zero-copy reads. [`Clone`] the item to outlive `'q`.
    ///
    /// ### Guidance
    ///
    /// Each filter is applied in the order of declaration. Filters are lazy and short-circuiting:
    /// enclosing filters **never** reassess buffers that are already excluded by upstream filters.
    /// Users are advised to declare more restrictive filters early to reduce the result set quickly
    /// and minimise work for subsequent filters.
    ///
    /// ### Implementation
    ///
    /// Every chain begins from a [data source](Src) that borrows the candidate buffer set. Every
    /// adapter is [monomorphized][4] against the concrete item type. No [`IO`](io) is executed
    /// until a terminal method is called e.g. [`read`][5] or [`iter`][6].
    ///
    /// Refer to the [source trait documentation](super::Source) for more details.
    ///
    /// [1]: super::Buffer
    /// [2]: super::Filter
    /// [3]: super::iter::Adapter
    /// [4]: https://rustc-dev-guide.rust-lang.org/backend/monomorph.html
    /// [5]: super::Adapter::read
    /// [6]: super::Adapter::iter
    pub trait Resolve<'q>: Deref<Target = Src<'q>> {
        /// The [item filter chain](super::iter::Adapter) returned by [`resolve`](Self::resolve).
        type Ok;

        /// [Excludes](super::Exclude) candidate buffers from the [mask](BitBox) before
        /// [resolving](Resolve::resolve) into an [item filter chain][3] of the same shape.
        ///
        /// ### Guidance
        ///
        /// Each [filter](super::Filter) is applied in the order of declaration. Filters are lazy
        /// and short-circuiting: enclosing filters **never** reassess [buffers](super::Buffer) that
        /// are already excluded by upstream filters. Users are advised to declare more restrictive
        /// filters early to reduce the result set quickly and minimise work for subsequent filters.
        ///
        /// ### Errors
        ///
        /// - [`Error::Io`] if an error occurs during file [`IO`](io) or item deserialization.
        /// - [`Error::Number`] if a recorded item count exceeds [`usize`].
        ///
        /// Refer to the [`Src::try_exclude`] documentation for the underlying iteration method.
        fn resolve(self, mask: &mut BitBox) -> Result<Self::Ok, Error>;
    }
}

/* ----------------------------------------------------------------- Reduce Trait Implementation */

impl<'q, I> Reduce<'q> for Src<'q, I>
where
    I: Read + Clone + 'q,
    I::Src<'q>: Deserialize<'q, Ok = I::Src<'q>> + Reader<'q, I>,
{
    type Ok = Self;

    fn query(&self) -> &'q Query<'q> {
        self.query
    }

    /// [`Src`] is not a [`Filter`]; the `mask` is unaltered by definition.
    #[allow(unused_variables, reason = "query::Src includes all buffers")]
    fn reduce(self, mask: &mut BitBox) -> Result<Self, Error> {
        Ok(self)
    }
}


        fn retain<B, G, I>(&mut self, bounds: &[B], test: &G) -> Result<&mut Self, Error>
        where
            S::Item: Evaluate<I>,
            B: RangeBounds<I>,
            G: Fn(&I) -> bool,
            I: for<'de> Deserialize<'de, Ok = I> + PartialOrd,
        {
            self.source.retain(bounds, test)?;
            Ok(self)
        }
    }

    impl<S> Adapter for Skip<S>
    where
        S: Adapter,
    {
        type Item = S::Item;

        fn root<'a>(&'a self) -> &'a Root<'a, S::Item> {
            self.inner.root()
        }

        fn buffers(&mut self) -> &mut Vec<Buffer> {
            self.inner.buffers()
        }

        fn stream(&self) -> Result<impl Iterator<Item = Outcome<S::Item>>, Error> {
        }

        fn count(&self) -> u64 {
            self.inner.count().saturating_sub(self.skip)
        }

        fn retain<B, G, I>(&mut self, bounds: &[B], test: &G) -> Result<&mut Self, Error>
        where
            S::Item: Evaluate<I>,
            B: RangeBounds<I>,
            G: Fn(&I) -> bool,
            I: for<'de> Deserialize<'de, Ok = I> + PartialOrd,
        {
            self.inner.retain(bounds, test)?;
            Ok(self)
        }
    }

    impl<S> Adapter for Take<S>
    where
        S: Adapter,
    {
        type Item = S::Item;

        fn root<'a>(&'a self) -> &'a Root<'a, S::Item> {
            self.inner.root()
        }

        fn buffers(&mut self) -> &mut Vec<Buffer> {
            self.inner.buffers()
        }

        fn stream(&self) -> Result<impl Iterator<Item = Outcome<S::Item>>, Error> {
        }

        fn count(&self) -> u64 {
            self.inner.count().min(self.take)
        }

        fn retain<B, G, I>(&mut self, bounds: &[B], test: &G) -> Result<&mut Self, Error>
        where
            S::Item: Evaluate<I>,
            B: RangeBounds<I>,
            G: Fn(&I) -> bool,
            I: for<'de> Deserialize<'de, Ok = I> + PartialOrd,
        {

    /* ----------------------------------------------------------------- Column Trait Definition */

    pub trait Column: Adapter + Join + Sized {
        fn range<B, I>(mut self, bounds: B) -> Result<impl Column<Item = Self::Item>, Error>
        where
            B: RangeBounds<I>,
            Self::Item: Evaluate<I>,
            I: for<'de> Deserialize<'de, Ok = I> + PartialOrd,
        {
        }

        fn eq<I>(mut self, item: I) -> Result<impl Column<Item = Self::Item>, Error>
        where
            Self::Item: Evaluate<I>,
            I: for<'de> Deserialize<'de, Ok = I> + PartialOrd + BitMatch,
        {
        }

        fn ne<I>(self, item: I) -> Result<impl Column<Item = Self::Item>, Error>
        where
            Self::Item: Evaluate<I>,
            I: for<'de> Deserialize<'de, Ok = I> + PartialOrd + BitMatch,
        {
            self.filter(move |op: &I| BitMatch::ne(op, &item))
        }

        fn one_of<I, S>(mut self, items: S) -> Result<impl Column<Item = Self::Item>, Error>
        where
            S: IntoIterator<Item = I>,
            Self::Item: Evaluate<I>,
            I: for<'de> Deserialize<'de, Ok = I> + PartialOrd + BitMatch,
        {
        }

        fn one_of_set<I, S>(mut self, items: S) -> Result<impl Column<Item = Self::Item>, Error>
        where
            S: IntoIterator<Item = I>,
            Self::Item: Evaluate<I>,
            I: for<'de> Deserialize<'de, Ok = I> + PartialOrd + Eq + Hash,
        {
        }

        fn none_of<I, S>(self, items: S) -> Result<impl Column<Item = Self::Item>, Error>
        where
            S: IntoIterator<Item = I>,
            Self::Item: Evaluate<I>,
            I: for<'de> Deserialize<'de, Ok = I> + PartialOrd + BitMatch,
        {
        }

        fn none_of_set<I, S>(self, items: S) -> Result<impl Column<Item = Self::Item>, Error>
        where
            S: IntoIterator<Item = I>,
            Self::Item: Evaluate<I>,
            I: for<'de> Deserialize<'de, Ok = I> + PartialOrd + Eq + Hash,
        {
        }

        fn filter<F, I>(mut self, test: F) -> Result<impl Column<Item = Self::Item>, Error>
        where
            Self::Item: Evaluate<I>,
            F: Fn(&I) -> bool,
            I: for<'de> Deserialize<'de, Ok = I> + PartialOrd,
        {
        }

        fn is_some(self) -> impl Column<Item = Self::Item>
        where
            Self::Item: IsOption,
        {
            Filter::new(self, |item: Self::Item| match item.is_some() {
                true => Outcome::Include(item),
                false => Outcome::Exclude(item),
            })
        }

        #[allow(clippy::wrong_self_convention, reason = "consumes self to wrap")]
        fn is_none(self) -> impl Column<Item = Self::Item>
        where
            Self::Item: IsOption,
        {
            Filter::new(self, |item: Self::Item| match item.is_none() {
                true => Outcome::Include(item),
                false => Outcome::Exclude(item),
            })
        }

        fn skip(mut self, count: u64) -> impl Column<Item = Self::Item> {
            let skip = super::Column::skip(self.buffers(), count);
            Skip { inner: self, skip }
        }

        fn take(mut self, count: u64) -> impl Column<Item = Self::Item> {
            super::Column::take(self.buffers(), count);
            Take { inner: self, take: count }
        }

        fn item(self, index: u64) -> Result<Option<Self::Item>, Error> {
        }

        fn read(&self) -> Result<impl Iterator<Item = Result<Self::Item, io::Error>>, Error> {
            self.stream().map(Resolve::resolve)
        }

        fn unique(&self) -> Result<HashSet<Self::Item, Xxh3Builder>, Error>
        where
            Self::Item: Eq + Hash,
        {
        }

        fn index<N>(&self) -> Result<HashMap<Self::Item, N, Xxh3Builder>, Error>
        where
            Self::Item: Eq + Hash,
            N: Unsigned,
        {
            let iter = self.read()?;
            Query::intern(iter)
        }
    }

    /* ------------------------------------------------------------- Column Trait Implementation */

    impl<C> Column for C where C: Adapter + Join {}

    /* -------------------------------------------------------------- Reconcile Trait Definition */

    pub(crate) trait Reconcile {
        fn and<O>(&mut self, other: &mut O) -> Result<&mut Self, Error>
        where
            O: Adapter;
    }

    /* ---------------------------------------------------------- Reconcile Trait Implementation */

    impl<'q, I> Reconcile for Root<'q, I> {
        fn and<O>(&mut self, other: &mut O) -> Result<&mut Self, Error>
        where
            O: Adapter,
        {
        }
    }

    impl<S, F> Reconcile for Filter<S, F>
    where
        S: Reconcile,
    {
        fn and<O>(&mut self, other: &mut O) -> Result<&mut Self, Error>
        where
            O: Adapter,
        {
        }
    }

    impl<S> Reconcile for Skip<S>
    where
        S: Reconcile,
    {
        fn and<O>(&mut self, other: &mut O) -> Result<&mut Self, Error>
        where
            O: Adapter,
        {
        }
    }

    impl<S> Reconcile for Take<S>
    where
        S: Reconcile,
    {
        fn and<O>(&mut self, other: &mut O) -> Result<&mut Self, Error>
        where
            O: Adapter,
        {
        }
    }

    impl<A, B> Reconcile for super::Join<A, B>
    where
        A: Join,
        B: Column,
    {
        fn and<O>(&mut self, other: &mut O) -> Result<&mut Self, Error>
        where
            O: Adapter,
        {
    #[allow(
        private_bounds,
        reason = "sealed: Reconcile is unreachable outside the crate"
    )]
    pub trait Join: Reconcile + Sized {
        fn and<O>(mut self, mut other: O) -> Result<super::Join<Self, O>, Error>
        where
            O: Column,
        {
        }
    }

    /* --------------------------------------------------------------- Join Trait Implementation */

    impl<T> Join for T where T: Reconcile {}

    /* ------------------------------------------------------------------- Walk Trait Definition */

}

/* --------------------------------------------------------------------------- Stream Sub-Module */

pub mod stream {
    use std::iter;

    use memmap2::Mmap;

    use crate::io::{Deserialize, Error};
    use crate::manifest::Buffer;
    use crate::read::{Read, Reader};

    /* -------------------------------------------------------------------------- Public Exports */

    pub(crate) struct Root<'a, B> {
        buffers: B,
        mmap: &'a Mmap,
    }
    impl<'m, B> Root<'m, B> {
        pub(crate) const fn new(buffers: B, mmap: &'m Mmap) -> Self {
            Self { buffers, mmap }
        }

        /// Deserialize each buffer item exactly once, yielding one [`Result`] per item.
        ///
        /// Every per-buffer source is constructed **eagerly**, so sector and framing errors – and
        /// the single item of a compact buffer – surface here rather than mid-stream. Each source
        /// is deserialized exactly once, ahead of the variant split.
        ///
        /// The item lifetime binds to the memory map `'m`, decoupled from the buffer-iteration
        /// borrow `'b`, so a zero-copy borrowed item outlives the transient buffer cursor.
        ///
        /// ### Errors
        ///
        /// Returns [`Error::Truncated`] if a buffer sector extends beyond the memory map or a
        /// compact body yields no item, or any error raised while [deserializing](Deserialize) a
        /// per-buffer source.
        pub(crate) fn iter<I>(self) -> Result<impl Iterator<Item = Result<I, Error>> + 'b, Error>
        where
            B: 'b,
            'm: 'b,
            I: Read + Clone + 'm,
            I::Src<'m>: Deserialize<'m, Ok = I::Src<'m>> + Reader<'m, I>,
        {
            let mmap = self.mmap;
            let size = self.buffers.size_hint().0;
            let mut runs = Vec::with_capacity(size);
            for buffer in self.buffers {
                let mut bytes = buffer.sector().slice(mmap)?;
                let len: usize = buffer.count().try_into()?;
                let src = I::Src::deserialize(&mut bytes)?;
                let flow = if matches!(buffer, Buffer::Compact { .. }) {
                    let missing = Error::Truncated { expected: len, actual: usize::MIN };
                    let item = src.iter()?.next().transpose()?.ok_or(missing)?;
                    let repeated = iter::repeat_n(item, len).map(Ok);
                    Decode::Same(repeated)
                } else {
                    Decode::Each(src.iter()?.take(len))
                };
                runs.push(flow);
            }
            let items = runs.into_iter().flatten();
            Ok(items)
        }
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
            Src::new(iter::once(buffer), mmap).iter::<u32>()?.collect()
        }

        /* -------------------------------------------------------------------------- Unit Tests */

        /// [`iter`](Src::iter) yields every item of a [`Basic`](Buffer::Basic) buffer,
        /// truncated to the recorded `count`.
        #[test]
        fn decode_streams_standard_buffer() {
            let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
            let mmap = map(&bytes);
            let buffer = Buffer::Basic {
                buffer: sector(bytes.len()),
                count: NonZeroU64::new(3).expect("Zero rows"),
            };
            let items = drained(&buffer, &mmap).expect("Stream failed");
            assert_eq!(items, [10, 20, 30]);
        }

        /// [`iter`](Src::iter) resolves the single item of a [`Compact`](Buffer::Compact)
        /// buffer exactly once and repeats it `count` times.
        #[test]
        fn decode_repeats_compact_item() {
            let bytes = vec![7u32].serialize().expect("Serialize failed");
            let mmap = map(&bytes);
            let buffer = Buffer::Compact {
                buffer: sector(bytes.len()),
                count: NonZeroU64::new(3).expect("Zero rows"),
            };
            let items = drained(&buffer, &mmap).expect("Stream failed");
            assert_eq!(items, [7, 7, 7]);
        }

        /// A sector extending beyond the memory map surfaces **eagerly** from stream construction
        /// rather than mid-iteration; no item is ever yielded.
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

    use bitvec::vec::BitVec;
    use memmap2::MmapMut;

    use super::column::{Adapter, Column as _};
    use super::*;
    use crate::accumulate::{Accumulate, OptBitVec, OptInSitu, Seq};
    use crate::{Sector, Serialize};

    /// Collect the [`Include`](Outcome::Include) items from a stream, dropping
    /// [`Exclude`](Outcome::Exclude) and panicking on a failed eager construction or any
    /// [`Error`](Outcome::Error).
    fn collected<I, S>(stream: Result<S, Error>) -> Vec<I>
    where
        S: Iterator<Item = Outcome<I>>,
    {
        stream
            .expect("Stream construction failed")
            .filter_map(|outcome| match outcome {
                Outcome::Include(item) => Some(item),
                Outcome::Exclude(..) => None,
                Outcome::Error(error) => panic!("Read error → {error}"),
            })
            .collect()
    }

    /// A [`Sector`] spanning the one `u32` at element `slot` of a serialized body.
    fn stat(slot: u64) -> Sector {
        let width = size_of::<u32>() as u64;
        Sector::new(slot * width, width).expect("Sector::new failed")
    }

    /// Build a [`Detailed`](manifest::Buffer::Detailed) `u32` descriptor over a serialized body,
    /// pointing `min` and `max` at the real items held at those element slots.
    fn detailed(len: usize, count: u64, min: u64, max: u64) -> manifest::Buffer {
        manifest::Buffer::Detailed {
            buffer: Sector {
                offset: u64::MIN,
                size: NonZeroU64::new(len as u64).expect("Empty body"),
            },
            count: NonZeroU64::new(count).expect("Zero rows"),
            min: stat(min),
            max: stat(max),
        }
    }

    /// Build a single-column `u32` [`Query`] named `v` whose descriptor carries real statistics.
    fn root(items: &[u32]) -> Query {
        let bytes = items.to_vec().serialize().expect("Serialize failed");
        let last = items.len() as u64 - 1;
        let buffer = detailed(bytes.len(), items.len() as u64, 0, last);
        with(&bytes, Type::U32, buffer)
    }

    /// Build a single-column [`Query`] named `v` over the provided serialized bytes and [`Buffer`].
    fn with(bytes: &[u8], ty: Type, buffer: manifest::Buffer) -> Query {
        let mut mmap = MmapMut::map_anon(bytes.len().max(1)).expect("Anonymous map failed");
        mmap[..bytes.len()].copy_from_slice(bytes);
        Query {
            mmap: Arc::new(mmap.make_read_only().expect("Read-only conversion failed")),
            columns: BTreeMap::from([(String::from("v"), Column { ty, buffers: vec![buffer] })]),
        }
    }

    /// Build a single-column [`Query`] named `v` over the provided serialized bytes; the descriptor
    /// is [`Basic`](manifest::Buffer::Basic), so it carries no statistics and is never pruned.
    fn query(bytes: &[u8], ty: Type, count: u64) -> Query {
        let buffer = manifest::Buffer::Basic {
            buffer: Sector {
                offset: u64::MIN,
                size: NonZeroU64::new(bytes.len() as u64).expect("Empty body"),
            },
            count: NonZeroU64::new(count).expect("Zero rows"),
        };
        with(bytes, ty, buffer)
    }

    /// Build a single-column [`Query`] named `v` whose descriptor is a
    /// [`Compact`](manifest::Buffer::Compact) buffer spanning one repeated item.
    fn compact(bytes: &[u8], ty: Type, count: u64) -> Query {
        let buffer = manifest::Buffer::Compact {
            buffer: Sector {
                offset: u64::MIN,
                size: NonZeroU64::new(bytes.len() as u64).expect("Empty body"),
            },
            count: NonZeroU64::new(count).expect("Zero rows"),
        };
        with(bytes, ty, buffer)
    }

    /// Build a two-column [`Query`] (`a` then `b`) with a distinct `u32` [`Buffer`] per column.
    fn pair(a: &[u32], b: &[u32]) -> Query {
        let ab = a.to_vec().serialize().expect("Serialize a");
        let bb = b.to_vec().serialize().expect("Serialize b");
        let mut mmap = MmapMut::map_anon(ab.len() + bb.len()).expect("Anonymous map failed");
        mmap[..ab.len()].copy_from_slice(&ab);
        mmap[ab.len()..].copy_from_slice(&bb);
        let buffer = |offset: usize, len: usize, count: usize| manifest::Buffer::Basic {
            buffer: Sector {
                offset: offset as u64,
                size: NonZeroU64::new(len as u64).expect("Empty body"),
            },
            count: NonZeroU64::new(count as u64).expect("Zero rows"),
        };
        Query {
            mmap: Arc::new(mmap.make_read_only().expect("Read-only conversion failed")),
            columns: BTreeMap::from([
                (
                    String::from("a"),
                    Column {
                        ty: Type::U32,
                        buffers: vec![buffer(0, ab.len(), a.len())],
                    },
                ),
                (
                    String::from("b"),
                    Column {
                        ty: Type::U32,
                        buffers: vec![buffer(ab.len(), bb.len(), b.len())],
                    },
                ),
            ]),
        }
    }

    #[test]
    fn column_round_trip() {
        let data: Vec<u32> = vec![10, 20, 30];
        let bytes = data.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 3);
        let rows = collected(query.column::<u32>("v").expect("Column failed").stream());
        assert_eq!(rows, data);
    }

    #[test]
    fn column_type_mismatch_errors() {
        let bytes = vec![1u32].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 1);
        assert!(matches!(
            query.column::<u16>("v").err(),
            Some(Error::Type { .. })
        ));
    }

    /// A [`Compact`](manifest::Buffer::Compact) descriptor decodes its single item once and repeats
    /// it exactly `count` times without further file access.
    #[test]
    fn compact_column_decodes_once() {
        let bytes = vec![7u32].serialize().expect("Serialize failed");
        let query = compact(&bytes, Type::U32, 3);
        let rows = collected(query.column::<u32>("v").expect("Column failed").stream());
        assert_eq!(rows, [7, 7, 7]);
    }

    /// A range **containing** the item of a [`Compact`](manifest::Buffer::Compact) column retains the
    /// buffer and repeats the item; a disjoint range prunes it eagerly instead of repeating an
    /// [`Exclude`](Outcome::Exclude) outcome to exhaust it.
    #[test]
    fn compact_repeats_contained_range() {
        let bytes = vec![7u32].serialize().expect("Serialize failed");
        let query = compact(&bytes, Type::U32, 3);
        let handle =
            query.column::<u32>("v").expect("Column failed").range(5u32..10).expect("range");
        assert_eq!(handle.buffers().len(), 1); // the item falls inside the range
        assert_eq!(collected(handle.stream()), [7, 7, 7]);
    }

    /// Every value filter evaluates a [`Compact`](manifest::Buffer::Compact) item **exactly** at
    /// query time, so a provably excluded compact buffer is pruned before any streaming.
    #[test]
    fn compact_prunes_disjoint_item() {
        let bytes = vec![7u32].serialize().expect("Serialize failed");
        let away = compact(&bytes, Type::U32, 3);
        let away = away.column::<u32>("v").expect("Column failed").eq(100u32).expect("eq");
        assert!(away.buffers().is_empty()); // pruned before iteration
        assert!(collected(away.stream()).is_empty());
        let kept = compact(&bytes, Type::U32, 3);
        let kept = kept.column::<u32>("v").expect("Column failed").eq(7u32).expect("eq");
        assert_eq!(collected(kept.stream()), [7, 7, 7]);
    }

    /// Inequality proves nothing from a statistic range, but prunes a
    /// [`Compact`](manifest::Buffer::Compact) buffer whose item is bit-identical to the operand.
    #[test]
    fn compact_prunes_ne() {
        let bytes = vec![7u32].serialize().expect("Serialize failed");
        let away = compact(&bytes, Type::U32, 3);
        let away = away.column::<u32>("v").expect("Column failed").ne(7u32).expect("ne");
        assert!(away.buffers().is_empty()); // every item is rejected
        let kept = compact(&bytes, Type::U32, 3);
        let kept = kept.column::<u32>("v").expect("Column failed").ne(9u32).expect("ne");
        assert_eq!(collected(kept.stream()), [7, 7, 7]);
    }

    /// A [`Basic`](manifest::Buffer::Basic) buffer carries no statistics: a range filter retains the
    /// buffer and filters its items at read time instead.
    #[test]
    fn basic_streams_unpruned() {
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 3);
        let handle =
            query.column::<u32>("v").expect("Column failed").range(15u32..25).expect("range");
        assert_eq!(handle.buffers().len(), 1); // never pruned
        assert_eq!(collected(handle.stream()), [20]); // filtered at read time
    }

    /// A compact `String` column resolves its framed composite item through the reader pipeline, so
    /// value filters prune and retain it correctly.
    #[test]
    fn string_filters_prune_compact() {
        let bytes = {
            let mut acc = Seq::<u8>::default();
            acc.push(String::from("red"));
            acc.serialize().expect("Serialize failed")
        };
        let away = compact(&bytes, Type::String, 3);
        let away = away
            .column::<String>("v")
            .expect("Column failed")
            .eq(String::from("blue"))
            .expect("eq");
        assert!(away.buffers().is_empty());
        let kept = compact(&bytes, Type::String, 3);
        let kept =
            kept.column::<String>("v").expect("Column failed").eq(String::from("red")).expect("eq");
        assert_eq!(collected(kept.stream()).len(), 3);
    }

    #[test]
    fn bool_column_round_trip() {
        let mut acc = BitVec::default();
        [true, false, true].into_iter().for_each(|bit| acc.push(bit));
        let bytes = acc.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::Bool, 3);
        let rows = collected(query.column::<bool>("v").expect("Column failed").stream());
        assert_eq!(rows, vec![true, false, true]);
    }

    #[test]
    fn opt_bit_vec_column_round_trip() {
        let mut acc = OptBitVec::<u32>::default();
        [Some(1u32), None, Some(3)].into_iter().for_each(|v| acc.push(v));
        let bytes = acc.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::option(Type::U32), 3);
        let rows = collected(query.column::<Option<u32>>("v").expect("Column failed").stream());
        assert_eq!(rows, vec![Some(1), None, Some(3)]);
    }

    #[test]
    fn niche_option_column_round_trip() {
        let mut acc = OptInSitu::<NonZeroU64>::default();
        [NonZeroU64::new(5), None, NonZeroU64::new(7)].into_iter().for_each(|v| acc.push(v));
        let bytes = acc.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::option(Type::NZU64), 3);
        let rows =
            collected(query.column::<Option<NonZeroU64>>("v").expect("Column failed").stream());
        assert_eq!(rows, vec![NonZeroU64::new(5), None, NonZeroU64::new(7)]);
    }

    #[test]
    fn seq_column_round_trip() {
        let mut acc = Seq::<u8>::default();
        acc.push(vec![97, 98, 99]);
        acc.push(vec![100, 101]);
        let bytes = acc.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::sequence(Type::U8), 2);
        let rows = collected(query.column::<Vec<u8>>("v").expect("Column failed").stream());
        assert_eq!(rows, vec![vec![97, 98, 99], vec![100, 101]]);
    }

    #[test]
    fn string_column_round_trip() {
        let mut acc = Seq::<u8>::default();
        acc.push("héllo".as_bytes().to_vec());
        acc.push("xyz".as_bytes().to_vec());
        let bytes = acc.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::String, 2);
        let rows = collected(query.column::<String>("v").expect("Column failed").stream());
        assert_eq!(rows, vec![String::from("héllo"), String::from("xyz")]);
    }

    #[test]
    fn str_column_zero_copy() {
        let mut acc = Seq::<u8>::default();
        acc.push(b"abc".to_vec());
        acc.push(b"de".to_vec());
        let bytes = acc.serialize().expect("Serialize failed");
        let query = query(&bytes, Type::String, 2);
        let rows = collected(query.column::<&str>("v").expect("Column failed").stream());
        assert_eq!(rows, vec!["abc", "de"]);
    }

    #[test]
    fn eq_filter_excludes_non_matching() {
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 3);
        let handle = query.column::<u32>("v").expect("Column failed").eq(20u32).expect("eq failed");
        assert_eq!(collected(handle.stream()), vec![20]);
    }

    #[test]
    fn ne_filter_excludes_matching() {
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 3);
        let handle = query.column::<u32>("v").expect("Column failed").ne(20u32).expect("ne failed");
        assert_eq!(collected(handle.stream()), vec![10, 30]);
    }

    #[test]
    fn set_membership_filters() {
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        let one = query(&bytes, Type::U32, 3);
        let one = one.column::<u32>("v").expect("Column").one_of([20u32, 30]).expect("one_of");
        assert_eq!(collected(one.stream()), [20, 30]);
        let none = query(&bytes, Type::U32, 3);
        let none = none.column::<u32>("v").expect("Column").none_of([20u32]).expect("none_of");
        assert_eq!(collected(none.stream()), [10, 30]);
    }

    /// [`is_some`](column::Column::is_some) retains [`Some`] rows; [`is_none`](column::Column::is_none)
    /// retains [`None`] rows, delegating validity to the optional mask.
    #[test]
    fn validity_filters_split_optionals() {
        let bytes = {
            let mut acc = OptBitVec::<u32>::default();
            [Some(1u32), None, Some(3)].into_iter().for_each(|v| acc.push(v));
            acc.serialize().expect("Serialize failed")
        };
        let some = query(&bytes, Type::option(Type::U32), 3);
        let some = some.column::<Option<u32>>("v").expect("Column").is_some();
        assert_eq!(collected(some.stream()), vec![Some(1), Some(3)]);
        let none = query(&bytes, Type::option(Type::U32), 3);
        let none = none.column::<Option<u32>>("v").expect("Column").is_none();
        assert_eq!(collected(none.stream()), vec![None]);
    }

    /// A value filter on an optional column tests each [`Some`]; a [`None`] item carries no operand
    /// to test and is **excluded**. Chaining `is_some` is therefore redundant, whereas `is_none`
    /// selects the absent items instead.
    #[test]
    fn value_filter_excludes_none_on_optional() {
        let bytes = {
            let mut acc = OptBitVec::<u32>::default();
            [Some(1u32), None, Some(20)].into_iter().for_each(|v| acc.push(v));
            acc.serialize().expect("Serialize failed")
        };
        let ty = || Type::option(Type::U32);
        let kept = query(&bytes, ty(), 3);
        let kept = kept.column::<Option<u32>>("v").expect("Column").eq(20u32).expect("eq");
        assert_eq!(collected(kept.stream()), vec![Some(20)]);
        let chained = query(&bytes, ty(), 3);
        let chained =
            chained.column::<Option<u32>>("v").expect("Column").eq(20u32).expect("eq").is_some();
        assert_eq!(collected(chained.stream()), vec![Some(20)]); // no further effect
        let absent = query(&bytes, ty(), 3);
        let absent = absent.column::<Option<u32>>("v").expect("Column").is_none();
        assert_eq!(collected(absent.stream()), vec![None]);
    }

    #[test]
    fn eq_type_mismatch_errors() {
        let bytes = vec![1u32].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 1);
        assert!(query.column::<bool>("v").is_err());
    }

    /// An [`eq`](column::Column::eq) disjoint from the buffer statistics prunes it; the handle empties
    /// and its stream is empty.
    #[test]
    fn eq_prunes_disjoint_column() {
        let query = root(&[10u32, 15, 20]);
        let handle = query.column::<u32>("v").expect("Column").eq(100u32).expect("eq failed");
        assert!(handle.buffers().is_empty());
        assert!(collected(handle.stream()).is_empty());
    }

    #[test]
    fn column_unknown_name_errors() {
        let bytes = vec![1u32].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 1);
        assert!(matches!(
            query.column::<u32>("missing").err(),
            Some(Error::Column { .. })
        ));
    }

    /// Two handles filter independently: filtering `a` leaves `b` untouched.
    #[test]
    fn handles_filter_per_column() {
        let query = pair(&[10, 20, 30], &[1, 2, 3]);
        let a = query.column::<u32>("a").expect("Column a").range(15u32..25).expect("range");
        assert_eq!(collected(a.stream()), [20]);
        assert_eq!(
            collected(query.column::<u32>("b").expect("Column b").stream()),
            [1, 2, 3]
        );
    }

    /// [`join`](column::Column::join) intersects the tagged buffer lists of two handles; a value
    /// filter that prunes one side prunes the sibling on sync.
    #[test]
    fn join_syncs_buffers() {
        // Column `a` carries restrictive statistics [10, 30]; `b` spans the full range. An
        // `eq(100)` on `a` is provably disjoint, so its sole buffer is pruned before the join.
        let bytes = vec![10u32, 20, 30].serialize().expect("Serialize failed");
        let mut mmap = MmapMut::map_anon(bytes.len()).expect("Anonymous map failed");
        mmap[..bytes.len()].copy_from_slice(&bytes);
        let sector = Sector {
            offset: u64::MIN,
            size: NonZeroU64::new(bytes.len() as u64).expect("Empty"),
        };
        let count = NonZeroU64::new(3).expect("Zero rows");
        // Column `a` carries real statistics resolved from the map; `b` carries none.
        let stats = detailed(bytes.len(), 3, 0, 2);
        let basic = manifest::Buffer::Basic { buffer: sector, count };
        let query = Query {
            mmap: Arc::new(mmap.make_read_only().expect("Read-only conversion failed")),
            columns: BTreeMap::from([
                (
                    String::from("a"),
                    Column { ty: Type::U32, buffers: vec![stats] },
                ),
                (
                    String::from("b"),
                    Column { ty: Type::U32, buffers: vec![basic] },
                ),
            ]),
        };
        let a = query.column::<u32>("a").expect("Column a").eq(100u32).expect("eq"); // prunes a
        let b = query.column::<u32>("b").expect("Column b");
        let (a, b) = a.join(b).expect("join failed").unpack();
        assert!(a.buffers().is_empty()); // the disjoint buffer is dropped
        assert!(b.buffers().is_empty()); // and intersected out of the sibling
    }

    /// A [`join`](column::Column::join) across handles from different queries is rejected.
    #[test]
    fn cross_query_join_errors() {
        let one = pair(&[1, 2], &[3, 4]);
        let two = pair(&[1, 2], &[3, 4]);
        let a = one.column::<u32>("a").expect("Column a");
        let b = two.column::<u32>("b").expect("Column b");
        assert!(matches!(a.join(b).err(), Some(Error::Join { .. })));
    }

    /// [`Column::get`](column::Column::get) windows a handle by positional slot without deserializing
    /// outside the window; [`item`](column::Column::item) extracts one slot.
    #[test]
    fn column_get_and_item() {
        let bytes = vec![10u32, 20, 30, 40].serialize().expect("Serialize failed");
        let query = query(&bytes, Type::U32, 4);
        let window = query.column::<u32>("v").expect("Column").get(1..3).expect("get failed");
        assert_eq!(collected(window.stream()), [20, 30]);
        let item = query.column::<u32>("v").expect("Column").item(3).expect("item failed");
        assert_eq!(item, 40);
    }

    /// [`Query::get`](Query::get) windows the whole query before extraction; each extracted column
    /// sees the identical slot window.
    #[test]
    fn query_get_windows_lockstep() {
        let query = pair(&[10, 20, 30], &[1, 2, 3]).get(1..3).expect("get failed");
        assert_eq!(
            collected(query.column::<u32>("a").expect("Column a").stream()),
            [20, 30]
        );
        assert_eq!(
            collected(query.column::<u32>("b").expect("Column b").stream()),
            [2, 3]
        );
    }

    /// [`Window::locate`] resolves half-open ranges over cumulative buffer counts, spanning a
    /// boundary, and rejects an empty range.
    #[test]
    fn window_locate_resolves_ranges() {
        let counts = [3u64, 3];
        let across = Window::locate(&counts, 2, 5).expect("window");
        assert_eq!(
            (across.first, across.last, across.skip, across.take.get()),
            (0, 1, 2, 3)
        );
        let inside = Window::locate(&counts, 1, 2).expect("window");
        assert_eq!(
            (inside.first, inside.last, inside.skip, inside.take.get()),
            (0, 0, 1, 1)
        );
        assert!(Window::locate(&counts, 3, 3).is_none());
    }
}
