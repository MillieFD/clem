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
//! Each new [`Query`] begins with every [`Column`](column::Column) and every [`Buffer`] from the
//! specified [`Schema`]. Individual columns can be resolved and filtered to subtractively reduce
//! the result set. Some filters are evaluated eagerly **before** file IO; removing individual
//! [buffers](Buffer) using [manifest] statistics. Other filters are attached to read-time
//! [adapters](column::Adapter) and evaluated lazily **during** [deserialization](Deserialize).
//!
//! A [`Query`] is a factory for strongly-typed [`Column`](column::Column) handles over one schema,
//! plus a set of unfiltered composite conveniences. Extraction is selection: a column is read only
//! when a handle is opened for it via [`column`](Query::column). Filters live on the handle as
//! concrete typed state and are applied to each item **after** deserialization, so every item is
//! deserialized exactly once and every predicate is an infallible, statically-dispatched test.
//!
//! ```rust,ignore
//! let overheating = dataset
//!     .query("schema_name")?
//!     .column::<f64>("temperature")?
//!     .range(35.0..)?
//!     .read();
//! ```
//!
//! Items are deserialized exactly once when the lazy [`Iterator`] returned by a terminal method is
//! polled. Every filter is an infallible monomorphized test.

#![doc = include_str!("../../../doc/query-filters.md")]
#![doc = include_str!("../../../doc/query-columns.md")]

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::fmt::{self, Display};
use std::hash::Hash;
use std::iter;
use std::num::TryFromIntError;
use std::sync::Arc;

use funty::Unsigned;
use memmap2::Mmap;
use minicbor::{CborLen, Decode, Encode};
use xxhash_rust::xxh3::Xxh3Builder;

use crate::io::{self, Deserialize};
use crate::manifest::{self, Buffer};
use crate::read::{Composite, Outcome, Read, Reader, Resolve};
use crate::schema::{Schema, Type, Unfolder, number};

/* ------------------------------------------------------------------------------ Public Exports */

/// A composable query interface to [read](Read) data from any [msca](crate) file; initialised from
/// [`Dataset::query`][1] and executed lazily when [`read`](Self::read) is iterated.
///
/// [`Query`] also provides a [`Column`](column::Column) factory for the specified [`Schema`].
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
    /// Map each **distinct** [item](I) to the corresponding on-disk [index](N).
    ///
    /// The [`Dataset`][1] is read in ascending insertion order; items record their first index and
    /// subsequent duplicate items are discarded.
    ///
    /// ### Errors
    ///
    /// - [`Error::Number`] if a first-occurrence index overflows [`N`].
    /// - [`Error::Io`] if a deserialization failure occurs.
    ///
    /// [1]: crate::dataset::Dataset
    pub fn unique<I, N>(&self) -> Result<HashMap<I, N, Xxh3Builder>, Error>
    where
        N: Unsigned,
        I: Read + Eq + Hash + 'static,
        for<'q> I::Src<'q>: Composite<'q, Query<'q>> + Iterator<Item = Outcome<I>> + 'q,
    {
        let iter = self.read::<I>()?;
        Self::intern(iter)
    }

    /// Intern each **distinct** [item](I) from the provided [set](S) and map to the corresponding
    /// [index](N) of the earliest on-disk occurrence.
    ///
    /// The index increments for each on-disk item. Repeated items intern to the index of their
    /// earlier occurrence while the counter advances `+1` for each duplicate. The maximum index is
    /// therefore greater than or equal to `≥` the number of [`HashMap`] entries.
    ///
    /// ### Errors
    ///
    /// - [`Error::Number`] if an index overflows [`N`].
    /// - [`Error::Io`] if a deserialization failure occurs.
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
            if let Entry::Vacant(entry) = map.entry(item?) {
                entry.insert(i);
            }
            next = i.checked_add(N::ONE);
        }
        Ok(map)
    }

    /// Select a named [`Column`] from the parent [`Query`].
    ///
    /// The requested type is [verified](Column::exact) against the actual on-disk column [`Type`]
    /// exactly once. Subsequent column operations – such as filtering and deserialization – can
    /// progress fearlessly without further runtime checks.
    ///
    /// ```rust,ignore
    /// .column::<f64>("temperature")? // a typed handle over the "temperature" column
    /// ```
    ///
    /// ### Errors
    ///
    /// - [`Error::Column`] if `name` is not found in the query [`BTreeMap`].
    /// - [`Error::Type`] if the requested [`Type`] does not match the on-disk [`Column`] type.
    pub fn column<'q, I>(&'q self, name: &str) -> Result<impl column::Column<Item = I> + 'q, Error>
    where
        I: Read + Clone + 'q,
        I::Src<'q>: Deserialize<'q, Ok = I::Src<'q>> + Reader<'q, I>,
        Schema: Unfolder<I>,
    {
        if let Some(e) = self.columns.get_key_value(name) {
            let column = e.1.exact::<I>()?.into();
            let src = column::Src::new(self, e.0, column);
            Ok(src)
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
        I: Read + 'q,
        I::Src<'q>: Composite<'q, Query<'q>> + Iterator<Item = Outcome<I>> + 'q,
    {
        let skip = self
            .columns
            .values_mut()
            .map(|col| Buffer::skip(&mut col.buffers, index))
            .last()
            .unwrap_or_default() as usize;
        self.read::<I>()?.nth(skip).transpose().map_err(Error::from)
    }

    pub fn stream<'q, I>(&'q self, name: &str) -> Result<impl Iterator<Item = Outcome<I>>, Error>
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
            .values();
        let src = stream::Root::new(buffers, &self.mmap);
        let items = src.stream()?.map(Outcome::from);
        Ok(items)
    }

    pub fn read<'q, I>(&'q self) -> Result<impl Iterator<Item = Result<I, io::Error>> + 'q, Error>
    where
        I: Read + 'q,
        I::Src<'q>: Composite<'q, Query<'q>> + Iterator<Item = Outcome<I>> + 'q,
    {
        I::Src::new(self).map(Resolve::resolve)
    }

    /// Returns the total number of on-disk items for this [`Schema`] across all [segments][1]; the
    /// sum of [`Buffer::count`] for one [`Column`].
    ///
    /// [1]: crate::segment::Segment
    pub fn count(&self) -> u64 {
        self.columns.values().next().map(Column::count).unwrap_or_default()
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

/// The result of **combining** two data sources.
///
/// `Join` is itself a data source over the two inner sources, enabling the construction of nested
/// join-trees over an arbitrary number of underlying [columns](column::Column).
///
/// Refer to the [join trait documentation](column::Join) for more details.
#[derive(Clone, Debug)]
#[non_exhaustive] // rejects external struct literal construction
pub struct Join<A, B>
where
    A: column::Join,
    B: column::Column,
{
    /// A single [`Column`](column::Column) or nested [`Join`] instance.
    pub a: A,
    /// A single [`Column`](column::Column) instance.
    pub b: B,
}

impl<A, B> Join<A, B>
where
    A: column::Join,
    B: column::Column,
{
    /// Consume `self` and return `a` and `b` which are guaranteed to contain **only**
    /// [buffers](Buffer) from corresponding [segments][1].
    ///
    /// [1]: crate::segment::Segment
    pub fn unpack(self) -> (A, B) {
        (self.a, self.b)
    }

    /// Returns an [`Iterator`] that yields [`deserialized`][1] composite [`items`](I) from the
    /// [joined][2] columns.
    ///
    /// ### Errors
    ///
    /// - [`Error::Column`] if the composite [`I`] requests a column that is not in the [`Join`].
    /// - [`Error::Type`] if the requested [`Type`] does not match the on-disk [`Column`] type.
    ///
    /// Refer to the [composite trait documentation](Composite) for more details.
    ///
    /// [1]: Deserialize::deserialize
    /// [2]: column::Join
    pub fn read<'a, I>(&'a self) -> Result<impl Iterator<Item = Result<I, io::Error>>, Error>
    where
        I: Read + 'a,
        I::Src<'a>: Composite<'a, Self> + Iterator<Item = Outcome<I>> + 'a,
    {
        I::Src::new(self).map(Resolve::resolve)
    }
}

/// A minimal column **descriptor** for [`Query`] planning and execution.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Eq, PartialEq, Encode, Decode, CborLen)]
pub struct Column {
    /// The [`Type`] of items contained within this [`Column`].
    #[n(0)]
    pub(crate) ty: Type,
    /// [`Buffer`] descriptors keyed by segment ordinal.
    #[cbor(n(1), skip_if = "BTreeMap::is_empty")]
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub(crate) buffers: BTreeMap<usize, Buffer>,
}

impl Column {
    /// Returns [`Error::Type`] if the requested [`Type`] does not match the on-disk [`Column`]
    /// type; otherwise returns an immutable reference to [`self`](Column) for method chaining.
    ///
    /// ### Guidance
    ///
    /// Refer to [`Column::accepts`] if a direct **or** nested inner-type match is permissible. Use
    /// [`Column::exact_mut`] if a mutable reference is required for downstream functions.
    pub fn exact<I>(&self) -> Result<&Self, Error>
    where
        Schema: Unfolder<I>,
    {
        let expect = Schema::unfold();
        match self.ty == expect {
            true => Ok(self),
            false => Error::Type { expect, actual: self.ty.clone() }.into(),
        }
    }

    /// Returns [`Error::Type`] if the requested [`Type`] does not match the on-disk [`Column`]
    /// type; otherwise returns a mutable reference to [`self`](Column) for method chaining.
    ///
    /// ### Guidance
    ///
    /// Refer to [`Column::accepts`] if a direct **or** nested inner-type match is permissible. Use
    /// [`Column::exact`] if an immutable reference is required for downstream functions.
    pub fn exact_mut<I>(&mut self) -> Result<&mut Self, Error>
    where
        Schema: Unfolder<I>,
    {
        self.exact::<I>()?;
        Ok(self)
    }

    /// Returns [`Error::Type`] if the requested [`Type`] does not match the on-disk [`Column`]
    /// type **or** nested inner subtype; otherwise returns an immutable reference to
    /// [`self`](Column) for method chaining.
    ///
    /// ### Guidance
    ///
    /// Refer to [`Type::exact`] if a direct non-nested match is required. Use
    /// [`Column::accepts_mut`] if a mutable reference is required for downstream functions.
    pub fn accepts<I>(&self) -> Result<&Self, Error>
    where
        Schema: Unfolder<I>,
    {
        let inner = Schema::unfold();
        match self.ty == inner || matches!(&self.ty, Type::Option { subtype: s } if **s == inner) {
            true => Ok(self),
            false => Error::Type { expect: inner, actual: self.ty.clone() }.into(),
        }
    }

    /// Returns [`Error::Type`] if the requested [`Type`] does not match the on-disk [`Column`]
    /// type **or** nested inner subtype; otherwise returns a mutable reference to [`self`](Column)
    /// for method chaining.
    ///
    /// ### Guidance
    ///
    /// Refer to [`Type::exact`] if a direct non-nested match is required. Use [`Column::accepts`]
    /// if an immutable reference is required for downstream functions.
    pub fn accepts_mut<I>(&mut self) -> Result<&mut Self, Error>
    where
        Schema: Unfolder<I>,
    {
        self.accepts()?;
        Ok(self)
    }

    /// Skip the fist `n` items.
    ///
    /// Eagerly removes [buffers](Buffer) from the [`Query`] according to their `count` statistic;
    /// returning the residual offset to the requested position within the first retained buffer.
    fn skip<N>(buffers: &mut Vec<Buffer>, n: N) -> N
    where
        N: Unsigned,
    {
        let (skip, count) = buffers
            .iter()
            .scan(N::MIN, |total, buf| {
                let count: N = buf.count().try_into().ok()?;
                *total = total.checked_add(count)?;
                Some(*total)
            })
            .take_while(|total| *total <= n)
            .zip(1..)
            .last()
            .unwrap_or_default();
        buffers.drain(..count);
        n - skip
    }
    fn take(buffers: &mut Vec<Buffer>, n: u64) {
        use std::ops::AddAssign;
        let count = buffers
            .iter()
            .scan(u64::MIN, |total, buf| {
                let count = buf.count();
                total.add_assign(count);
                Some(*total)
            })
            .take_while(|start| *start < n)
            .count();
        buffers.truncate(count);
    }
    /// Map the provided [`Key`](String) to a new empty [`Column`].
    pub(crate) fn map(entry: (&String, &manifest::Column)) -> (String, Self) {
        (entry.0.clone(), entry.1.clone().into())
    }
}

impl From<manifest::Column> for Column {
    fn from(src: manifest::Column) -> Self {
        Self { ty: src.ty, buffers: src.buffers }
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
    /// Underlying [`io::Error`] from the [msca](crate) [file](io::File).
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
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Column { name } => write!(f, "Column '{name}' not found"),
            Self::Io(e) => write!(f, "Query IO error → {e}"),
            Self::Number(e) => write!(f, "Number error → {e}"),
            Self::Type { expect, actual } => write!(f, "Type error → {expect} ≠ {actual}"),
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

/* --------------------------------------------------------------------------- Column Sub-Module */

pub mod column {
    use std::collections::{HashMap, HashSet};
    use std::hash::Hash;
    use std::marker::PhantomData;
    use std::ops::{Not, RangeBounds, RangeInclusive};
    use std::sync::Arc;
    use std::{iter, slice};

    use funty::Unsigned;
    use xxhash_rust::xxh3::Xxh3Builder;

    use super::{Error, Query, stream};
    use crate::io::{self, Deserialize};
    use crate::manifest::Buffer;
    use crate::read::{Evaluate, IsOption, Outcome, Read, Reader, Resolve};
    use crate::schema::BitMatch;

    /* -------------------------------------------------------------------------- Public Exports */

    /// An [iteration](Iterator) **state machine** over [deserialized][1] [items](I) from one
    /// [`Column`] in the [`Query`] result set.
    ///
    /// [1]: Deserialize::deserialize
    #[doc(hidden)] // returned by Adapter::root; not intended as a stable API
    pub struct Src<'q, I> {
        /// An immutable reference to the parent [`Query`].
        pub query: &'q Query,
        /// The name of the [`Column`] over which this [`Src`] iterates.
        pub name: &'q str,
        /// Candidate [`Buffer`] descriptors keyed by [`Segment`](crate::segment::Segment) ordinal.
        pub buffers: BTreeMap<usize, Buffer>,
        /// Type-state carrier for the requested [`item`](I) type.
        item: PhantomData<I>,
    }

    impl<'q, I> Src<'q, I> {
        pub(crate) const fn new(query: &'q Query, name: &'q str, buffers: Vec<Buffer>) -> Self {
            Self { query, name, buffers, item: PhantomData }
        }
    }
    pub(crate) struct Filter<S, F> {
        source: S,
        filter: F,
    }

    impl<S, F> Filter<S, F> {
        const fn new(source: S, filter: F) -> Self {
            Self { source, filter }
        }
        fn test<I>(source: S, test: F) -> Filter<S, impl Fn(S::Item) -> Outcome<S::Item>>
        where
            S: Adapter,
            S::Item: Evaluate<I>,
            F: Fn(&I) -> bool,
        {
            Filter::new(source, move |item: S::Item| item.evaluate(&test))
        }
    }

    impl<S, F, E> Iterator for Filter<S, F>
    where
        S: Iterator<Item = Outcome<E>>,
        F: Fn(E) -> Outcome<E>,
    {
        type Item = Outcome<E>;

        fn next(&mut self) -> Option<Outcome<E>> {
            match self.source.next()? {
                Outcome::Include(item) => (self.filter)(item).into(),
                other => other.into(),
            }
        }
    }

    pub(crate) struct Skip<S> {
        inner: S,
        skip: u64,
    }

    pub(crate) struct Take<S> {
        inner: S,
        take: u64,
    }
    #[doc(hidden)] // reachable through the blanket Column implementation
    pub trait Adapter {
        type Item: Read;

        fn root<'a>(&'a self) -> &'a Root<'a, Self::Item>;

        fn buffers(&mut self) -> &mut Vec<Buffer>;

        fn stream(&self) -> Result<impl Iterator<Item = Outcome<Self::Item>>, Error>;

        fn count(&self) -> u64 {
            self.root().buffers.iter().map(Buffer::count).sum()
        }

        fn retain<B, F, I>(&mut self, bounds: &[B], test: &F) -> Result<&mut Self, Error>
        where
            Self::Item: Evaluate<I>,
            B: RangeBounds<I>,
            F: Fn(&I) -> bool,
            I: for<'de> Deserialize<'de, Ok = I> + PartialOrd;
    }

    /* ------------------------------------------------------------ Adapter Trait Implementation */

    impl<'q, I> Adapter for Root<'q, I>
    where
        I: Read + Clone + 'q,
        I::Src<'q>: Deserialize<'q, Ok = I::Src<'q>> + Reader<'q, I>,
    {
        type Item = I;

        fn root(&self) -> &Self {
            self
        }

        fn buffers(&mut self) -> &mut Vec<Buffer> {
            &mut self.buffers
        }

        fn stream(&self) -> Result<impl Iterator<Item = Outcome<I>>, Error> {
        }

        fn retain<B, F, O>(&mut self, bounds: &[B], test: &F) -> Result<&mut Self, Error>
        where
            I: Evaluate<O>,
            B: RangeBounds<O>,
            F: Fn(&O) -> bool,
            O: for<'de> Deserialize<'de, Ok = O> + PartialOrd,
        {
        }
    }

    impl<S, F> Adapter for Filter<S, F>
    where
        S: Adapter,
        F: Fn(S::Item) -> Outcome<S::Item>,
    {
        type Item = S::Item;

        fn root(&self) -> &Src<S::Item> {
            self.source.root()
        }

        fn buffers(&mut self) -> &mut Vec<Buffer> {
            self.source.buffers()
        }

        fn stream(&self) -> Result<impl Iterator<Item = Outcome<S::Item>>, Error> {
            Ok(Filter {
                source: self.source.stream()?,
                filter: &self.filter,
            })
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
