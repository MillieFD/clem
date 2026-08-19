/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! A composable [`Schema`] interface to read data from any [msca](crate) file.
//!
//! ---
//!
//! Each new `Schema` begins with every [`Column`](manifest::Column) and every [`Buffer`]:
//!
//! - Use [`Schema::read`] or [`Schema::iter`] to pull every item from every column without filters.
//! - Use [`Schema::column`] to extract individual columns which can then be [filtered](Filter).
//!
//! Filters subtractively reduce the result set. Filters can act at two points in the query
//! lifecycle: **buffer filters** are evaluated **before** file [`IO`](io); **item filters** are
//! evaluated **after** [deserialization](Deserialize). Every item is deserialized exactly once and
//! every infallible filter [`Fn`] is [monomorphized][1] by the compiler.
//!
//! ```rust,ignore
//! let overheating = dataset
//!     .query("schema_name")?
//!     .column::<f64>("temperature")?
//!     .range(35.0..)
//!     .iter();
//! ```
//!
//! Items are deserialized lazily each time [`next`](Iterator::next) is called on the [`Iterator`]
//! returned by a terminal method.
//!
//! [1]: https://rustc-dev-guide.rust-lang.org/backend/monomorph.html

#![doc = include_str!("../../doc/query-filters.md")]
#![doc = include_str!("../../doc/query-columns.md")]

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
use crate::item::{self, Composite, Outcome, Resolve};
use crate::iter::{self, Origin};
use crate::manifest::{self, Buffer};
use crate::mask;
use crate::read::{Decode, Decoder, Evaluate};
use crate::schema::{self, Type, Unfolder, number};

/* ------------------------------------------------------------------------------ Public Exports */

/// A **composable query interface** to [read](crate::read) data from any [msca](crate) file.
///
/// ### Lifetime
///
/// The lifetime `'d` is tied to the underlying [`Dataset`][1] from which `self` was [extracted][2].
///
/// Refer to the [module-level documentation](self) for implementation details.
///
/// [1]: crate::dataset::Dataset
/// [2]: crate::dataset::Dataset::query
#[derive(Clone, Copy, Debug)]
pub struct Schema<'d> {
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

impl<'d> Schema<'d> {
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
        I: item::Read<'d> + Eq + Hash + 'd,
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
    /// Refer to [`Schema::into_hash_map`] and [`Query::into_hash_map`] for the entry points.
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

    /// Select a named [`Column`](manifest::Column) from the parent [`Schema`].
    ///
    /// The requested type is verified against the actual on-disk column [`Type`] exactly once.
    /// Subsequent column operations – such as filtering and deserialization – proceed
    /// without further runtime checks.
    ///
    /// ```rust,ignore
    /// .column::<f64>("temperature")? // the typed "temperature" column
    /// ```
    ///
    /// ### Errors
    ///
    /// - [`Error::Column`] if `name` is not found in the [`Schema`](manifest::Schema).
    /// - [`Error::Type`] if the requested `Type` does not match the on-disk column type.
    pub fn column<I>(&self, name: &str) -> Result<Column<'d, I>, Error>
    where
        I: Decode<'d> + Clone + 'd,
        schema::Schema: Unfolder<I>,
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
    /// Returns [`None`] if `n` exceeds the number of on-disk items written for the
    /// [`Schema`](manifest::Schema).
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Io`] if an error occurs during file [`IO`](io) or item deserialization.
    pub fn nth<I>(self, n: usize) -> Result<Option<I>, Error>
    where
        I: item::Read<'d> + 'd,
    {
        I::nth(self, n)?.included().next().transpose().map_err(Error::from)
    }

    /// Return an [`Iterator`] yielding one [`Outcome`] per [deserialized][1] item, each rebuilt
    /// from every [`Column`](manifest::Column) the composite `I` names.
    ///
    /// The requested [`Type`] is verified against the on-disk column type exactly once. Subsequent
    /// deserialization proceeds without additional runtime checks.
    ///
    /// ### Guidance
    ///
    /// Use `Schema::read` when no filter is required. Use [`Schema::column`] to extract a named
    /// column when filters *are* required. An unfiltered extracted column retains every
    /// [`Buffer`], meaning both unfiltered forms yield the same items in the same order.
    ///
    /// ### Errors
    ///
    /// - [`Error::Column`] if a column named by the composite `I` is absent from the schema.
    /// - [`Error::Type`] if the requested type is incompatible with the on-disk column type.
    ///
    /// Refer to [`Schema::iter`] for a resolved alternative that automatically re-polls the
    /// iterator to yield only [included](Outcome::Include) items.
    ///
    /// [1]: Deserialize::deserialize
    pub fn read<I>(self) -> Result<impl Iterator<Item = Outcome<I>> + 'd, Error>
    where
        I: item::Read<'d> + 'd,
    {
        let items = I::read(self)?;
        Ok(items)
    }

    /// Returns an [`Iterator`] that yields [`Composite`] items.
    ///
    /// ### Implementation
    ///
    /// Each field of the composite is lazily [deserialized][1] from the respective [`Column`][2].
    /// Refer to the [composite read trait documentation](item::Read) for more details.
    ///
    /// ### Guidance
    ///
    /// The iterator automatically re-polls the [`Source`] until an [included](Outcome::Include)
    /// item is returned. Use [`Schema::read`] for a non-resolved alternative that yields
    /// [`Outcome`] instead.
    ///
    /// ### Errors
    ///
    /// - [`Error::Column`] if a column named by the composite `I` is absent from the schema.
    /// - [`Error::Type`] if the requested [`Type`] does not match the on-disk column type.
    ///
    /// Refer to [`Schema::read`] for a non-resolved alternative that yields [`Outcome`].
    ///
    /// [1]: Deserialize::deserialize
    /// [2]: manifest::Column
    pub fn iter<I>(self) -> Result<impl Iterator<Item = Result<I, io::Error>> + 'd, Error>
    where
        I: item::Read<'d> + 'd,
    {
        let iter = self.read::<I>()?.included();
        Ok(iter)
    }

    /// Returns the total number of on-disk items for this [`Schema`](manifest::Schema) across
    /// every segment; the sum of [`Buffer::count`] for one [`Column`](manifest::Column).
    pub fn count(self) -> u64 {
        self.schema.count()
    }
}

impl<'d> PartialEq for Schema<'d> {
    /// Returns `true` if two queries read the same [`Schema`](manifest::Schema).
    ///
    /// Read the [trait documentation](PartialEq) for more details.
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.schema, other.schema)
    }
}

impl<'d> Eq for Schema<'d> {}

/// An immutable **byte source** for one [`Column`] and all subsequent [adapters](Query).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Src<'d> {
    /// An immutable reference to the parent [`Schema`].
    pub(crate) query: Schema<'d>,
    /// [`Buffer`] descriptors for the [`Column`][1] across all segments in [`Sector`][2] order.
    ///
    /// [1]: manifest::Column
    /// [2]: io::Sector
    // NOTE: sector offset increases monotonically → sector order matches on-disk segment order
    pub(crate) buffers: &'d BTreeSet<Buffer>,
}

impl<'d> Src<'d> {
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
    pub fn mask(&self) -> BitBox {
        let n = self.buffers.len();
        BitVec::repeat(true, n).into_boxed_bitslice()
    }

    /// Returns the logical number of items recorded in each [`Buffer`].
    ///
    /// ### Errors
    ///
    /// Returns [`io::Error::Number`] if any recorded count overflows `usize`.
    pub(crate) fn counts(&self, mask: &BitBox) -> impl Iterator<Item = Result<usize, io::Error>> {
        let bits = mask.iter().by_vals();
        self.buffers.iter().zip(bits).map(|e| match e.1 {
            true => e.0.count().try_into().map_err(io::Error::from),
            false => Ok(usize::MIN),
        })
    }

    /// Returns **only** the [`Buffer`] descriptors included by the [mask] in [`Sector`][1] order.
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

/// A **strongly typed data source** for one [`Column`] and all subsequent [adapters](Query).
///
/// ### Implementation
///
/// The `Column` wrapper pins a generic [byte source](Src) to the specified item type `I` that is
/// [verified][1] exactly once against the actual on-disk column type at [construction][2]. This
/// design enables the compiler to [monomorphize][3] all subsequent operations and proceed without
/// runtime type checks.
///
/// [1]: manifest::Column::exact
/// [2]: Schema::column
/// [3]: https://rustc-dev-guide.rust-lang.org/backend/monomorph.html
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Column<'d, I> {
    /// The on-disk source from which [`self`](Column) reads.
    pub(crate) src: Src<'d>,
    /// Zero-sized **marker** carrying the item type.
    pub(crate) item: PhantomData<I>,
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
