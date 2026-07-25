/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

#![doc = include_str!("../../doc/manifest.md")]

use std::cmp::Ordering;
use std::collections::{BTreeMap, Bound};
use std::fmt::{Display, Formatter};
use std::hash::{Hash, Hasher};
use std::num::NonZeroU64;
use std::ops::RangeBounds;

use funty::Fundamental;
use memmap2::Mmap;
use minicbor::{CborLen, Decode, Encode};
use smol::io::{AsyncRead, AsyncReadExt, AsyncSeek};

use crate::io::{Checksum, Deserializer, Register};
use crate::schema::{number, Type};
use crate::segment::{Header, Segment, Variant};
use crate::{io, Deserialize, Sector, Serialize};

/* ------------------------------------------------------------------------------ Public Exports */

/// Manifest of file segments and accompanying metadata for random access and predicate pruning.
/// See the [module-level documentation](self) for details.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Default, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
#[cbor(tag(100))]
pub(crate) struct Manifest {
    /// [`Schema`] segments keyed by [`name`](String).
    #[cbor(n(0), skip_if = "BTreeMap::is_empty")]
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub schemas: BTreeMap<String, Schema>,
    /// [`Binary`](crate::binary::Bin) segments keyed by [`name`](String).
    #[cbor(n(1), skip_if = "BTreeMap::is_empty")]
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub bins: BTreeMap<String, Sector>,
    /// Implementers can use the optional free-form metadata sector to attach file-level
    /// domain-specific information such as:
    ///
    /// - Date and time
    /// - Experimental parameters
    /// - Provenance
    ///
    /// If a metadata section is included in the file, a corresponding `length` and `offset` are
    /// described in the `manifest`. The core library includes a basic read and write surface, but
    /// implementers must include their own metadata parsing and validation logic.
    #[cfg(feature = "metadata")]
    #[cbor(n(1), skip_if = "Option::is_none")]
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub metadata: Option<Sector>,
}

impl Manifest {
    /// [`Deserialize`] a file [`Manifest`] from the provided [`File`](AsyncRead) at the specified
    /// [`Sector`], verifying the segment framing recorded by the [write-cycle](io).
    ///
    /// ### Errors
    ///
    /// - [`Error::Truncated`][1] if the sector length is too small to contain a segment [`Header`].
    /// - [`Error::Checksum`][2] if computed checksum does not match the on-disk checksum suffix.
    /// - [`Error::Decode`][3] from the underlying manifest [`CBOR`](minicbor) decode operation.
    /// - [`Error::Io`][4] from the underlying [`seek`][5] and [`read`][6] operations.
    ///
    /// [1]: io::Error::Truncated
    /// [2]: io::Error::Checksum
    /// [3]: io::Error::Decode
    /// [4]: io::Error::Io
    /// [5]: Sector::seek_to_start
    /// [6]: AsyncReadExt::read_exact
    pub async fn from_file<F>(file: &mut F, sector: Sector) -> Result<Self, io::Error>
    where
        F: AsyncRead + AsyncSeek + Unpin + ?Sized,
    {
        let size = sector.size.get().try_into()?;
        let mut buf = vec![0u8; size];
        sector.seek_to_start(file).await?;
        file.read_exact(&mut buf).await?;
        Manifest::verify(&buf)?
            .get(Header::SIZE..)
            .ok_or_else(|| io::Error::Truncated {
                expected: Header::SIZE,
                actual: buf.len(),
            })?
            .deserialize_into()
    }

    /// Reconstruct a [`Manifest`] by walking the self-describing segment region.
    ///
    /// Used to recover a corrupt or truncated manifest by replaying intact segments. Each segment
    /// header is decoded sequentially and re-registered in a fresh [`Manifest`].
    #[allow(unused)]
    pub fn rebuild(data: &[u8], tail: NonZeroU64) -> Self {
        unimplemented!("Manifest::rebuild is not yet implemented")
    }

    /// Returns the corresponding [entry](S::Entry) in [`self`](Self) for the provided [`Segment`].
    ///
    /// Refer to the [trait documentation](Register) for more details.
    // noinspection RsNeedlessLifetimes → explicit 'm lifetime improves readability
    pub fn entry<'m, S>(&'m mut self, seg: &S) -> Result<S::Entry<'m>, S::Error>
    where
        S: Register,
    {
        seg.entry(self)
    }
}

impl Segment for Manifest {
    const VARIANT: Variant = Variant::Manifest;

    #[allow(unused_variables, reason = "manifest segment is not aligned")]
    fn wrap(&self, offset: u64) -> Result<Vec<u8>, number::Error> {
        use crate::io::Buffer;
        const PREFIX: usize = Header::SIZE + size_of::<u64>(); // Σ bytes before the segment body
        let size = self.size()?.get();
        let full = size.as_usize().checked_add(PREFIX).ok_or(number::Error::Zero)?;
        let mut buf = vec![u8::MIN; full];
        buf.as_mut_slice()
            .serialize_push(&{ Self::VARIANT as u8 })?
            .serialize_push(&size)?
            .serialize_push(self)?;
        Self::checksum(&mut buf)?;
        Ok(buf)
    }
}

impl Checksum for Manifest {}

impl Serialize for Manifest {
    type Buffer = Vec<u8>;

    fn size(&self) -> Result<NonZeroU64, number::Error> {
        let size: u64 = minicbor::len(self).try_into()?;
        size.try_into().map_err(number::Error::Convert)
    }

    fn serialize_into<'a>(&self, mut buf: &'a mut [u8]) -> Result<&'a mut [u8], number::Error> {
        // SAFETY: minicbor::encode is infallible when writing to &mut [u8]
        minicbor::encode(self, &mut buf).expect("Infallible manifest CBOR encode failed");
        Ok(buf)
    }

    fn serialize(&self) -> Result<Self::Buffer, number::Error> {
        // NOTE: Scoped trait import avoids namespace conflict with Buffer struct (below)
        use crate::io::Buffer;
        let size = self.size()?.get().try_into()?;
        let buf = vec![0u8; size].serialize_push(self)?;
        // NOTE: cannot use static assertion as size is dependent on runtime data accumulation.
        debug_assert_eq!(buf.len(), size, "actual size ≠ predicted size");
        Ok(buf)
    }
}

impl<'de> Deserialize<'de> for Manifest {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, io::Error> {
        // NOTE: one-shot decode from a pre-sized CBOR buffer; the slice is not advanced.
        minicbor::decode(src).map_err(io::Error::Decode)
    }
}

/// A minimal schema segment **descriptor** that specifies:
///
/// 1. [`Sector`] where the schema segment is located on disk.
/// 2. [`BTreeMap`] of [`Column`] descriptors keyed by name.
///
/// This type does **not** contain the actual schema definition or columnar data buffers; it is a
/// lightweight descriptor for segment discovery and access without holding buffer contents in
/// memory. An on-disk schema segment encodes the schema definition (column names and types) while
/// on-disk data segments contain the columnar buffers.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
#[non_exhaustive] // rejects external struct literal construction
pub struct Schema {
    /// Location of the [`Schema`] segment.
    #[n(0)]
    pub sector: Sector,
    /// [`Column`] descriptors keyed by name.
    ///
    /// The [`BTreeMap`] guarantees a stable deterministic column order for consistent binary
    /// encoding and schema comparison.
    #[cbor(n(1), skip_if = "BTreeMap::is_empty")]
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub columns: BTreeMap<String, Column>,
}

impl Schema {
    /// Returns the total number of items across every [`Segment`] for this [`Schema`].
    ///
    /// Calculated from the [`Manifest`] via the summation (Σ) of [`Buffer::count`] for one
    /// [`Column`] – since all columns in a single segment contain the same number of logical items.
    pub(crate) fn count(&self) -> u64 {
        self.columns
            .values()
            .next()
            .into_iter()
            .flat_map(|column| &column.buffers)
            .map(Buffer::count)
            .sum()
    }
}

/// A minimal column **descriptor** wrapping a collection of [`Buffer`] descriptors.
///
/// This type does **not** contain the actual buffer data; it is a lightweight descriptor for column
/// discovery and access without holding buffer contents in memory. Data is stored via one or more
/// on-disk data segments, each of which contains a buffer for this column.
///
/// [`Vec`] order in-memory is **not** guaranteed to reflect [`Sector`] order on-disk.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Eq, Encode, Decode, CborLen)]
#[non_exhaustive] // rejects external struct literal construction
pub struct Column {
    /// The [`Type`] of values contained within this column.
    #[n(0)]
    pub ty: Type,
    /// List of [`Buffer`] descriptors for this column across all data segments.
    #[cbor(n(1), skip_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub buffers: Vec<Buffer>,
}

impl PartialEq for Column {
    fn eq(&self, other: &Self) -> bool {
        self.ty == other.ty
    }
}

impl Ord for Column {
    fn cmp(&self, other: &Self) -> Ordering {
        self.ty.cmp(&other.ty)
    }
}

impl PartialOrd for Column {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hash for Column {
    fn hash<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        self.ty.hash(state);
    }
}

impl From<Type> for Column {
    fn from(ty: Type) -> Self {
        Column { ty, buffers: Vec::new() }
    }
}

/// A minimal columnar buffer **descriptor** that specifies:
///
/// 1. [`Sector`] where the buffer is located on disk.
/// 2. Logical number of data entries e.g. for index arithmetic.
/// 3. Statistics such as `min` and `max` for predicate pruning.
///
/// This type does **not** contain the actual buffer data; it is a lightweight descriptor for buffer
/// discovery and access without holding buffer contents in memory. Data is stored via contiguous
/// buffers distributed across one or more on-disk data segments.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
#[doc(hidden)] // Reachable through Accumulate::buffers for the #[derive(Data)] macro.
pub enum Buffer {
    /// A compact buffer containing exactly **one** item repeated `count` times.
    #[n(0)]
    #[non_exhaustive] // rejects external struct literal construction
    Compact {
        /// Location of the [`Buffer`] on disk.
        ///
        /// Sector `offset` is calculated relative to the immutable segment region, excluding the
        /// [file](io::File) [header](io::Header). Refer to the [write-cycle](self) documentation
        /// for more details.
        #[n(0)]
        buffer: Sector,
        /// Logical number of repetitions of the single [Serialized](Serialize) item.
        ///
        /// Empty buffers are never written to disk; this invariant is enforced by [`NonZeroU64`].
        #[n(1)]
        count: NonZeroU64,
    },
    /// A buffer containing **more than one** distinct item with no orderable statistics.
    #[n(1)]
    #[non_exhaustive] // rejects external struct literal construction
    Basic {
        /// Location of the [`Buffer`] on disk.
        ///
        /// Sector `offset` is calculated relative to the immutable segment region, excluding the
        /// [file](io::File) [header](io::Header). Refer to the [write-cycle](self) documentation
        /// for more details.
        #[n(0)]
        buffer: Sector,
        /// Number of data entries.
        ///
        /// Empty buffers are never written to disk; this invariant is enforced by [`NonZeroU64`].
        #[n(1)]
        count: NonZeroU64,
    },
    /// A buffer containing **more than one** distinct [`PartialOrd`] item.
    #[n(2)]
    #[non_exhaustive] // rejects external struct literal construction
    Detailed {
        /// Location of the [`Buffer`] on disk.
        ///
        /// Sector `offset` is calculated relative to the immutable segment region, excluding the
        /// [file](io::File) [header](io::Header). Refer to the [write-cycle](self) documentation
        /// for more details.
        #[n(0)]
        buffer: Sector,
        /// Number of data entries.
        ///
        /// Empty buffers are never written to disk; this invariant is enforced by [`NonZeroU64`].
        #[n(1)]
        count: NonZeroU64,
        /// Location of the **minimum** item recorded in this buffer; used to filter whole segments.
        ///
        /// The [`Sector`] spans **exactly one** serialized item within the [`Buffer`] body;
        /// [`Deserialize`] the item directly to use for segment-level evaluation.
        #[n(2)]
        min: Sector,
        /// Location of the **maximum** item recorded in this buffer; used to filter whole segments.
        ///
        /// The [`Sector`] spans **exactly one** serialized item within the [`Buffer`] body;
        /// [`Deserialize`] the item directly to use for segment-level evaluation.
        #[n(3)]
        max: Sector,
    },
}

impl Buffer {
    /// Returns the logical number of items recorded in [`self`](Buffer).
    pub(crate) const fn count(&self) -> u64 {
        match self {
            Buffer::Detailed { count, .. }
            | Buffer::Compact { count, .. }
            | Buffer::Basic { count, .. } => count.get(),
        }
    }

    /// Returns the [`Sector`] recorded in [`self`](Buffer).
    pub(crate) const fn sector(&self) -> &Sector {
        match self {
            Buffer::Compact { buffer, .. }
            | Buffer::Basic { buffer, .. }
            | Buffer::Detailed { buffer, .. } => buffer,
        }
    }

    /// Returns `true` if [`self`](Buffer) is provably disjoint from the specified [`Bounds`][1].
    ///
    /// - [`Buffer::Detailed`] is evaluated using `min` and `max` statistics resolved from disk.
    /// - [`Buffer::Compact`] and [`Buffer::Basic`] carry no statistics; never provably disjoint.
    ///
    /// A compact buffer [`Sector`] spans exactly **one** on-disk item that is resolved and
    /// evaluated at read-time. The compact body may contain a [`Composite`][2] item that is
    /// computationally-expensive to [`Deserialize`]. Compact buffer exclusion is therefore assessed
    /// through the common [`Reader`](crate::Reader) pipeline instead of the bare statistic path
    /// used here. This behaviour may change in future releases.
    ///
    /// ### ⚠️ Safety
    ///
    /// This function is marked as [unsafe][3] due to the potential for undefined behaviour if the
    /// requested type [`I`] does not match the actual [`Column`](Column) [`Type`].
    ///
    /// ### Errors
    ///
    /// - [`Error::Truncated`][4] if a statistic sector extends beyond the memory map
    /// - [`io::Error`] if an error occurs while [deserializing](Deserialize) a statistic from disk.
    ///
    /// [1]: RangeBounds
    /// [2]: crate::read::Composite
    /// [3]: https://doc.rust-lang.org/book/ch20-01-unsafe-rust.html
    /// [4]: io::Error::Truncated
    pub(crate) unsafe fn disjoint<I, B>(&self, bounds: &B, mmap: &Mmap) -> Result<bool, io::Error>
    where
        B: RangeBounds<I>,
        I: for<'de> Deserialize<'de, Ok = I> + PartialOrd,
    {
        let (min, max) = match self {
            Buffer::Detailed { min, max, .. } => (min, max),
            Buffer::Compact { .. } | Buffer::Basic { .. } => return Ok(false),
        };
        let min: I = min.slice(mmap)?.deserialize_into()?;
        let max: I = max.slice(mmap)?.deserialize_into()?;
        let above = match bounds.end_bound() {
            Bound::Included(inc) => &min > inc,
            Bound::Excluded(exc) => &min >= exc,
            Bound::Unbounded => false,
        };
        let below = match bounds.start_bound() {
            Bound::Included(inc) => &max < inc,
            Bound::Excluded(exc) => &max <= exc,
            Bound::Unbounded => false,
        };
        Ok(above || below)
    }
}

/* ------------------------------------------------------------------------------ Specific Error */

/// Errors returned by [`Manifest`] operations such as segment registration or retrieval.
///
/// Enum variants cover various granular error cases that may arise when working with the file
/// manifest. Users should consider handling errors explicitly wherever possible to provide
/// meaningful error messages and recovery actions.
///
/// ### Implementation
///
/// This enum is `#[non_exhaustive]` meaning additional variants may be added in future versions.
/// Implementers are advised to include a wildcard arm `_` to account for potential additions.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Encode, Decode, CborLen)]
#[non_exhaustive] // To accommodate potential future error cases.
pub enum Error {
    /// No entry is recorded in the [`Manifest`] under the requested [`name`](String).
    #[n(0)]
    NotFound {
        /// Requested [`name`](String) with no corresponding [`Manifest`] entry.
        #[n(0)]
        name: String,
    },
    /// An entry is already recorded in the [`Manifest`] under the requested [`name`](String).
    ///
    /// Segments are immutable once written. Registration can only fill [vacant entries][1].
    /// Collisions are detected **before** file [`IO`][2] occurs.
    ///
    /// [1]: std::collections::btree_map::VacantEntry
    /// [2]: io::File::write
    #[n(1)]
    Collision {
        /// Requested [`name`](String) shared by the new and existing [`Manifest`] entries.
        #[n(0)]
        name: String,
    },
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { name } => write!(f, "Manifest entry not found → {name}"),
            Self::Collision { name } => write!(f, "Manifest entry collision → {name}"),
        }
    }
}

impl std::error::Error for Error {}

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
    use memmap2::MmapMut;

    use super::*;

    /* ---------------------------------------------------------------------------- Shared State */

    /// Build a read-only anonymous [`Mmap`] over `bytes`, backing the statistic resolution tests.
    fn map(bytes: &[u8]) -> Mmap {
        let mut mmap = MmapMut::map_anon(bytes.len().max(1)).expect("Anonymous map failed");
        mmap[..bytes.len()].copy_from_slice(bytes);
        mmap.make_read_only().expect("Read-only conversion failed")
    }

    /* ------------------------------------------------------------------------------ Unit Tests */

    /// A manifest segment round-trips: [`frame`](Segment::wrap) then [`verify`](Checksum::verify)
    /// the checksum and [`deserialize`](Deserialize::deserialize) the payload to recover the
    /// original [`Manifest`].
    #[test]
    fn manifest_segment_round_trips() {
        let manifest = Manifest::default();
        let bytes = manifest.wrap(0).expect("Wrap failed");
        let region = Manifest::verify(&bytes).expect("Checksum failed");
        let out = Manifest::deserialize(&mut &region[Header::SIZE..]).expect("Deserialize failed");
        assert_eq!(out, manifest);
    }

    /// Corrupting any framed byte is detected by [`verify`](Checksum::verify) as
    /// [`io::Error::Checksum`].
    #[test]
    fn manifest_checksum_detects_corruption() {
        let mut bytes = Manifest::default().wrap(0).expect("Frame failed");
        bytes[Header::SIZE] ^= u8::MAX; // flip the first payload byte
        let err = Manifest::verify(&bytes).expect_err("Corruption undetected");
        assert!(matches!(err, io::Error::Checksum));
    }

    /// A region shorter than one trailing checksum is rejected with [`io::Error::Truncated`].
    #[test]
    fn manifest_verify_rejects_short_region() {
        let err = Manifest::verify([u8::MIN; 4].as_slice()).expect_err("Short region accepted");
        assert!(matches!(err, io::Error::Truncated { .. }));
    }

    /// Every [`Buffer`] variant round-trips through its tagged CBOR representation.
    #[test]
    fn buffer_cbor_round_trips() {
        let buffer = Sector::new(8u64, 16u64).expect("Sector::new failed");
        let count = NonZeroU64::new(3).expect("Count is zero");
        let detailed = Buffer::Detailed {
            buffer,
            count,
            min: Sector::new(8u64, 4u64).expect("Sector::new failed"),
            max: Sector::new(20u64, 4u64).expect("Sector::new failed"),
        };
        let compact = Buffer::Compact { buffer, count };
        let basic = Buffer::Basic { buffer, count };
        for buf in [detailed, compact, basic] {
            let mut bytes = vec![u8::MIN; minicbor::len(&buf)];
            let mut sink = bytes.as_mut_slice();
            // SAFETY: minicbor::encode is infallible when writing to &mut [u8]
            minicbor::encode(&buf, &mut sink).expect("Infallible buffer CBOR encode failed");
            let out: Buffer = minicbor::decode(&bytes).expect("Buffer CBOR decode failed");
            assert_eq!(out, buf);
        }
    }

    /// [`Detailed`](Buffer::Detailed) resolves its statistic sectors against the memory map and
    /// deserializes each as exactly one item: `[10, 30]` is disjoint from `100..200` but overlaps
    /// `20..40`.
    #[test]
    fn detailed_buffer_disjoint_by_statistics() {
        let bytes = [10u32.to_le_bytes(), 30u32.to_le_bytes()].concat();
        let mmap = map(&bytes);
        let width = size_of::<u32>() as u64;
        let detailed = Buffer::Detailed {
            buffer: Sector::new(0u64, bytes.len() as u64).expect("Sector::new failed"),
            count: NonZeroU64::new(2).expect("Count is zero"),
            min: Sector::new(0u64, width).expect("Sector::new failed"),
            max: Sector::new(width, width).expect("Sector::new failed"),
        };
        // 100..200 is entirely above [10, 30] → buffer is removed
        assert!(unsafe { detailed.disjoint(&(100u32..200), &mmap) }.expect("Disjoint failed"));
        // 20..40 straddles [10, 30] → buffer is retained
        assert!(!unsafe { detailed.disjoint(&(20u32..40), &mmap) }.expect("Disjoint failed"))
    }

    /// [`Compact`](Buffer::Compact) and [`Basic`](Buffer::Basic) descriptors carry no statistics
    /// and are never provably disjoint; a compact item is instead evaluated exactly by a filter.
    #[test]
    fn compact_and_basic_are_never_disjoint() {
        let mmap = map(&[u8::MIN]);
        let buffer = Sector::new(8u64, 16u64).expect("Sector::new failed");
        let count = NonZeroU64::new(3).expect("Count is zero");
        for buf in [
            Buffer::Compact { buffer, count },
            Buffer::Basic { buffer, count },
        ] {
            // SAFETY: both variants return before any type-dependent statistic is deserialized
            let disjoint = unsafe { buf.disjoint(&(10u32..20), &mmap) }.expect("Disjoint failed");
            assert!(!disjoint);
        }
    }

    /// [`Schema::count`] sums the item counts across every buffer of the first column, spanning all
    /// three descriptor variants.
    #[test]
    fn schema_count_sums_every_buffer() {
        let sector = Sector::new(8u64, 16u64).expect("Sector::new failed");
        let detailed = Buffer::Detailed {
            buffer: sector,
            count: NonZeroU64::new(3).expect("Count is zero"),
            min: sector,
            max: sector,
        };
        let compact = Buffer::Compact {
            buffer: sector,
            count: NonZeroU64::new(2).expect("Count is zero"),
        };
        let basic = Buffer::Basic {
            buffer: sector,
            count: NonZeroU64::new(4).expect("Count is zero"),
        };
        let column = Column {
            ty: Type::U32,
            buffers: vec![detailed, compact, basic],
        };
        let schema = Schema {
            sector,
            columns: BTreeMap::from([(String::from("v"), column)]),
        };
        assert_eq!(schema.count(), 9);
    }
}
