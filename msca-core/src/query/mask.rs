/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! The **buffer mask adapter chain** evaluated during file [`IO`](io).
//!
//! Each adapter tests whole buffers – rather than individual items – and excludes candidates that
//! are provably disjoint from the requested results set.
//!
//! ### Implementation
//!
//! [`Buffer`] inclusion is described using a positional [mask](BitBox) where the `n`th [bit][1]
//! corresponds to the `n`th buffer from the `n`th data [segment][2].
//!
//! ```text
//! buffers   [ A ][ B ][ C ][ D ][ E ]    Immutable borrowed buffer set.
//! mask        1    0    1    1    0      Mutable owned bitmask.
//!             ▼         ▼    ▼
//! read        A         C    D           Buffers B and E are never read.
//! ```
//!
//! Each [`filter`] is applied subtractively to reduce the mask.
//!
//! Every chain begins from a [data source](Src) that borrows the candidate [`Buffer`] set. The
//! terminal method builds a [mask] that initially includes every candidate buffer. This mask is
//! passed along the chain, with each adapter assessing surviving buffers against a filter to
//! exclude candidates that are provably disjoint from the requested results set. Every adapter is
//! [monomorphized][2] against the concrete item type.
//!
//! Refer to the [query lifecycle documentation](Source) for more information.
//!
//! [1]: bitvec::ptr::BitPtr
//! [2]: crate::segment::Segment

use std::hash::Hash;
use std::marker::PhantomData;
use std::ops::{Deref, Not, RangeBounds};

use bitvec::boxed::BitBox;

use super::*;
use crate::io::Deserialize;
use crate::read::{Evaluate, IsOption, Read};

/* ------------------------------------------------------------------------------ Public Exports */

/// A [column](manifest::Column) [adapter](Adapter) that skips the first `n` items.
///
/// This adapter is initialised via [`Adapter::skip`] and excludes any [buffers](Buffer) that are
/// provably disjoint from the requested result set.
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
/// This adapter is initialised via [`Adapter::take`] and excludes any [buffers](Buffer) that are
/// provably disjoint from the requested result set.
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

/// A [column][1] [adapter](Adapter) retaining only items from `S` that are **not** present in `K`.
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

/* -------------------------------------------------------------------- Resolve Trait Definition */

/// A [buffer](Buffer) [filter](Filter) chain that reduces the candidate buffer [mask] before
/// [resolving](Resolve::resolve) into an [item filter chain][1] of the same shape.
///
/// ### Lifetime
///
/// This trait carries a `'d` lifetime from the [`Dataset`][2] to ensure that no item outlives the
/// file from which it was [deserialized](Deserialize). This design enables zero-copy reads.
/// [`Clone`] the item to outlive `'d`.
///
/// ### Implementation
///
/// Each [`filter`] wraps the [data source](Source) in an [`Adapter`] that captures the necessary
/// state to assess whole [buffers](Buffer) and individual items. Successive filters therefore
/// construct a nested adapter chain. Terminal methods e.g. [`Adapter::read`] lazily convert the
/// whole chain into a nested [`Iterator`] chain of the same shape. This trait determines the
/// [buffer adapter](mask) → [item adapter](iter) state transition.
///
/// Refer to the [source trait documentation](Source) for more details.
///
/// ### Guidance
///
/// Each filter is applied in the order of declaration. Filters are lazy and short-circuiting:
/// enclosing filters **never** reassess buffers that are already excluded by upstream filters.
/// Users are advised to declare more restrictive filters upstream to reduce the result set quickly
/// and minimise work for downstream filters.
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

/* ---------------------------------------------------------------- Resolve Trait Implementation */

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
        let test = |op: &K::Item| keys.contains(op).not();
        source.with_item(mask, test)?;
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
