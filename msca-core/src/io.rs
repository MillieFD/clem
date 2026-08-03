/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! Low-level IO for reading and writing files.
//!
//! ---
//!
//! The [msca](crate) format maximises performance by separating the data lifecycle into two phases:
//!
//! 1. **In-memory** accumulator optimised for high-throughput ingestion.
//! 2. **On-disk** columnar buffers optimised for range-based querying across arbitrary dimensions.
//!
//! This module coordinates the transition between memory and disk phases to ensure data durability
//! and efficient access patterns suitable for edge deployment on resource-constrained hardware.
//! The on-disk layout minimises contention for multiple-producer multiple-consumer workflows.
//!
//! ### Segment Composition
//!
//! Each file is partitioned into self-describing [segments](Segment) which are immutable once
//! written. Each segment begins with a minimal header consisting of a [`variant`][1] identifier
//! and [`length`](NonZeroU64).
//!
//! - [`Schema`](crate::Schema) segments describe the structure of encoded data.
//! - `Data` segments carry columnar buffers for a specified schema instance.
//!
//! Multimodality and schema evolution are realised by appending additional schema segments. Data
//! storage and file extensibility are realised by appending additional data segments. Format
//! extensibility may be achieved via the introduction of new segment variants in future releases.
//!
//! ### Lazy Partial Reads
//!
//! On-disk data is represented using a [`Sector`] instance prior to file IO. This design ensures:
//!
//! - **O(1) Random Access:** Readers `seek` directly to the relevant file region.
//! - **Efficient:** Readers `take` the required number of bytes instead of loading the entire file.
//!
//! Passing a small `Sector` instance can reduce overhead compared to passing an owned data buffer.
//! Sectors enforce the immutability of underlying on-disk data; implementers must [`Copy`] into an
//! owned type when mutability is required e.g. for downstream data processing.
//!
//! ### Manifest
//!
//! A self-describing **CBOR** file [`Manifest`] is included after the immutable segment region and
//! lists all file segments by type. The manifest acts like the index of a book to enhance segment
//! discovery and enable O(1) random access. Refer to the [manifest documentation][2] for details.
//!
//! ### Metadata
//!
//! An optional free-form `metadata` [`Sector`] may be written after the [`Manifest`] where
//! implementers can attach file-level domain-specific information such as:
//!
//! - Date and time
//! - Experimental parameters
//! - Provenance
//!
//! The [`Manifest`] may include an optional `metadata` field which points to this [`Sector`]. The
//! file IO mechanisms defined in this module will always preserve metadata and update the
//! [`Manifest`] metadata field during the write-cycle if present, but will only provide a read and
//! write surface if the corresponding metadata feature is enabled. Implementers must include their
//! own metadata parsing and validation logic.
//!
//! [1]: segment::Variant

#![doc = include_str!("../../doc/write-cycle.md")]
#![doc = include_str!("../../doc/read-cycle.md")]

use std::array::TryFromSliceError;
use std::convert::{Infallible, TryInto};
use std::io::SeekFrom;
use std::num::{self, NonZeroU64, TryFromIntError};
use std::ops::Add;
use std::path::{Path, PathBuf};
use std::str::Utf8Error;
use std::{fmt, mem};

use bitvec::order::Lsb0;
use bitvec::slice::BitSlice;
use bitvec::view::BitView;
use memmap2::{Mmap, MmapOptions};
use minicbor::{CborLen, Decode, Encode};
use smol::fs::{self, OpenOptions};
use smol::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWriteExt};
use xxhash_rust::xxh3::xxh3_64;

use crate::manifest::{self, Manifest};
use crate::segment::{self, Align, Segment};
use crate::{Serialize, number, schema};

/* ------------------------------------------------------------------------------ Public Exports */

/// Magic byte sequence used to identify a valid [msca](crate) file.
const MAGIC: [u8; 4] = *b"msca";

/// Current [msca](crate) major version number which is embedded in the file header to indicate
/// breaking changes in the format specification. Forwards and backwards compatibility across
/// version numbers is not guaranteed. Implementers must reject any unrecognised version number.
const VERSION: u8 = 0;

/// Total length of the file header in bytes. Includes the [magic][1] bytes, [version][2] number,
/// and zero-filled [alignment](segment) bytes.
///
/// [1]: MAGIC
/// [2]: VERSION
pub const HEADER: usize = size_of_val(&MAGIC) + size_of_val(&VERSION) + size_of::<Header>() + ALIGN;

/// Number of trailing zero bytes required to align the [`File`](File) [`Header`] to the next
/// [64-bit boundary](segment) for SIMD vectorisation support.
const ALIGN: usize = {
    let n = size_of_val(&MAGIC) + size_of_val(&VERSION) + size_of::<Header>() & 7;
    n.next_multiple_of(8).saturating_sub(n)
};

/// A contiguous byte region within the [msca](crate) file.
///
/// Implementers must [`Copy`] into an owned type when mutability is required e.g. for downstream
/// data processing.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
#[non_exhaustive] // rejects external struct literal construction; use Sector::new
pub struct Sector {
    /// Byte offset to the start of the sector.
    #[n(0)]
    pub offset: u64,
    /// Total size of the sector in bytes.
    #[n(1)]
    pub size: NonZeroU64,
}

impl Sector {
    /// Construct a [`Sector`] spanning `size` bytes from the specified `offset`.
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Number`] if `length` is zero or either argument overflows its target type.
    pub fn new<A, B>(offset: A, size: B) -> Result<Self, Error>
    where
        A: TryInto<u64>,
        B: TryInto<NonZeroU64>,
        Error: From<A::Error> + From<B::Error>,
    {
        Ok(Self {
            offset: offset.try_into()?,
            size: size.try_into()?,
        })
    }

    /// [`Seek`](AsyncSeek::poll_seek) the provided bytes stream to the start of [`self`](Sector)
    /// and return the new position.
    ///
    /// Returns [`Error::Io`] if the underlying seek operation fails.
    pub async fn seek_to_start<F>(&self, stream: &mut F) -> Result<u64, Error>
    where
        F: AsyncSeek + Unpin + ?Sized,
    {
        stream.seek(SeekFrom::Start(self.offset)).await.map_err(Error::from)
    }

    /// Returns the offset immediately following [`self`](Sector), or [`None`] on `u64` overflow.
    pub const fn next(&self) -> Option<NonZeroU64> {
        self.size.checked_add(self.offset)
    }

    /// Resolve a [`Sector`] **relative** to the specified offset.
    ///
    /// ### Errors
    ///
    /// Returns [`number::Error::Zero`] if the resolved offset overflows `u64`.
    pub(crate) fn relative(mut self, origin: u64) -> Result<Self, number::Error> {
        self.offset = self.offset.checked_add(origin).ok_or(number::Error::Zero)?;
        Ok(self)
    }

    /// [`Sector`] spanning the single fixed-`width` item selected from `items` by the `better`
    /// comparison, or [`None`] if no item is present.
    ///
    /// Each candidate is paired with its element index, so the located sector spans exactly one
    /// serialized item at `index × width`. An absent [`None`] item carries no operand to compare and
    /// is skipped **without** disturbing the element index of its neighbours.
    ///
    /// ### Guidance
    ///
    /// Comparison follows standard [`PartialOrd`] semantics; no ordering is invented for items that
    /// [`PartialOrd`] leaves incomparable. An item that compares `false` against every other –
    /// IEEE-754 `NaN` – therefore wins only as the opening candidate. That outcome is conservative:
    /// a `NaN` statistic satisfies no bounded predicate, so [`disjoint`][1] proves nothing and the
    /// buffer is retained rather than pruned.
    ///
    /// [1]: crate::manifest::Buffer::disjoint
    pub(crate) fn locate<I, S, O>(items: S, width: usize, cmp: O) -> Option<Self>
    where
        S: IntoIterator<Item = Option<I>>,
        O: Fn(&I, &I) -> bool,
    {
        let best = items
            .into_iter()
            .enumerate()
            .filter_map(|(index, item)| Some((index, item?))) // Absent items carry no operand
            .reduce(|best, next| match cmp(&next.1, &best.1) {
                true => next,
                false => best,
            })?;
        let width = u64::try_from(width).ok()?;
        let offset = u64::try_from(best.0).ok()?.checked_mul(width)?;
        Self::new(offset, width).ok()
    }

    /// Read the byte [slice][1] defined by [`self`](Sector) from the provided [`Mmap`].
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Truncated`] if the sector extends beyond the end of the [`Mmap`], or
    /// [`Error::Number`] if numeric conversion overflow occurs.
    ///
    /// [1]: https://doc.rust-lang.org/std/primitive.slice.html
    pub fn slice<'a>(&self, mmap: &'a Mmap) -> Result<&'a [u8], Error> {
        let start = self.offset.try_into()?;
        let end = self.next().ok_or(number::Error::Zero)?.get().try_into()?;
        mmap.get(start..end).ok_or_else(|| Error::Truncated {
            expected: end - start,
            actual: mmap.len().saturating_sub(start),
        })
    }
}

impl Add for Sector {
    type Output = Option<Self>;

    fn add(self, rhs: Self) -> Self::Output {
        let offset = self.offset.min(rhs.offset);
        let size = self.size.checked_add(rhs.size.get())?;
        Some(Self { offset, size })
    }
}

impl Serialize for Sector {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, number::Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(number::Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], number::Error> {
        buf.serialize_push(&self.offset)?.serialize_push(&self.size)
    }

    fn serialize(&self) -> Result<Self::Buffer, number::Error> {
        [u8::MIN; size_of::<Self>()].serialize_push(self)
    }
}

/// A **serialisation primitive** for size-prefixed file regions.
///
/// ### Data Layout
///
/// It is not possible to predetermine the on-disk space required for [unsized][1] file elements
/// such as [segments](segment) and [buffers][2]; the exact size depends upon runtime variables such
/// as the number of [accumulated](crate::accumulate) items.
///
/// The [msca](crate) format is **self-describing** to improve data integrity and file robustness.
/// Unsized regions therefore record a [`NonZeroU64`] size prefix that describes the exact number of
/// additional bytes required to [`Read`] the region.
///
/// ### Guidance
///
/// The **eight-byte** size prefix is **not included** in the recorded byte size. Readers should:
///
/// 1. Begin by deserializing the size prefix.
/// 2. Read the specified number of additional bytes.
/// 3. Remove **0-7** padding bytes from the end of the extracted [slice][3].
///
/// The removed bytes may include padding to the next [64-bit alignment boundary](Align). Empty
/// **zero-length** regions are never [written](Segment::write) to disk; the size prefix is
/// [non-zero](num::NonZero) to enforce this invariant.
///
/// [1]: https://doc.rust-lang.org/reference/dynamically-sized-types.html
/// [2]: crate::manifest::Buffer
/// [3]: https://doc.rust-lang.org/std/primitive.slice.html
#[doc(hidden)] // Reachable through the #[derive(Data)] macro; not part of the stable public API.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SizedBuf<I>(I);

impl<I> SizedBuf<I> {
    /// Wrap the provided [`item`](I) for **length-prefixed** serialization.
    pub const fn new(item: I) -> Self {
        Self(item)
    }
}

impl<I> SizedBuf<I> {
    /// Size of the [`NonZeroU64`] prefix in bytes.
    pub(crate) const PREFIX: u64 = size_of::<NonZeroU64>() as u64;
}

impl<I> From<I> for SizedBuf<I> {
    fn from(item: I) -> Self {
        Self(item)
    }
}

impl<I> AsRef<I> for SizedBuf<I> {
    fn as_ref(&self) -> &I {
        &self.0
    }
}

impl<I> AsRef<[u8]> for SizedBuf<I>
where
    I: AsRef<[u8]>,
{
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl<I> Serialize for SizedBuf<I>
where
    I: Serialize,
{
    type Buffer = Vec<u8>;

    fn size(&self) -> Result<NonZeroU64, number::Error> {
        self.as_ref()
            .size()?
            .get()
            .align()?
            .checked_add(Self::PREFIX)
            .and_then(NonZeroU64::new)
            .ok_or(number::Error::Zero)
    }

    fn serialize_into<'b>(&self, buf: &'b mut [u8]) -> Result<&'b mut [u8], number::Error> {
        let size = self.as_ref().size()?.get();
        let pad = size.pad()?;
        let buf = self.as_ref().serialize_into(size.serialize_into(buf)?)?;
        debug_assert!(buf.len() >= pad, "actual size < aligned size");
        buf[..pad].fill(u8::MIN);
        Ok(&mut buf[pad..])
    }

    fn serialize(&self) -> Result<Self::Buffer, number::Error> {
        let size = self.size()?.get().try_into()?;
        let buf = vec![0u8; size].serialize_push(self)?;
        // NOTE: cannot use static assertion as size is dependent on runtime data accumulation.
        debug_assert_eq!(buf.len(), size, "actual size ≠ predicted size");
        Ok(buf)
    }
}

impl<'de> Deserialize<'de> for SizedBuf<&'de [u8]> {
    type Ok = Self;

    /// Remove one **length-prefixed** [`SizedBuf`] from the front of the provided [source][1] and
    /// borrow the exact body; advancing the source **in-situ** past the extracted body and the
    /// zero-filled padding to the next 64-bit [alignment boundary](Align).
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Truncated`] if `src` contains fewer than the required number of bytes, or
    /// [`Error::Number`] if the on-disk `size` is not a valid [`NonZeroU64`].
    ///
    /// Refer to the [trait documentation](Deserialize::deserialize) for more information.
    ///
    /// [1]: https://doc.rust-lang.org/std/primitive.slice.html
    fn deserialize(src: &mut &'de [u8]) -> Result<Self::Ok, Error> {
        let size: usize = NonZeroU64::deserialize(src)?.get().try_into()?;
        let data = src
            .split_at_checked(size)
            .ok_or_else(|| Error::Truncated { expected: size, actual: src.len() })?;
        let pad = size.pad()?;
        *src = data.1.get(pad..).ok_or(Error::Truncated { expected: pad, actual: data.1.len() })?;
        Ok(Self(data.0))
    }
}

/// Mutable region of the file header.
///
/// Excludes immutable header elements such as the [magic bytes][1] and [version number][2]. See the
/// [module documentation](self) for a detailed description of the file header layout.
///
/// [1]: MAGIC
/// [2]: VERSION
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Hash, Encode, Decode, CborLen)]
pub(crate) struct Header {
    /// On-disk location of the encoded [`Manifest`] which is always immediately after the immutable
    /// segment region.
    ///
    /// New segments are appended from the manifest `offset` during the [write-cycle](crate::io).
    #[n(0)]
    pub manifest: Sector,
}

impl Header {
    /// [`Sector`] containing the mutable region of the file [`Header`]. Excludes the immutable
    /// [magic bytes][1] and [version number][2].
    ///
    /// [1]: MAGIC
    /// [2]: VERSION
    const SECTOR: Sector = Sector {
        offset: { size_of_val(&MAGIC) + size_of_val(&VERSION) } as u64,
        size: NonZeroU64::new(size_of::<Self>() as u64).expect("Length is zero"),
    };

    /// [`Deserialize`] the file [`Header`] using the provided file [`Reader`](AsyncRead).
    ///
    /// ### Error
    ///
    /// Returns [`Error::Io`] if the underlying [read operation][1] fails or the reader encounters
    /// an unexpected end of file.
    ///
    /// [1]: AsyncReadExt::poll_read
    async fn from_file<F>(file: &mut F) -> Result<Self, Error>
    where
        F: AsyncRead + Unpin + ?Sized,
    {
        let mut buf = [0u8; HEADER];
        file.read_exact(&mut buf).await?;
        Header::deserialize(&mut &buf[..])
    }
}

impl From<Sector> for Header {
    /// Construct a new [file](File) [header](Self) using the provided [`Manifest`] sector.
    ///
    /// Refer to the [trait](From) and [module](io) documentation for more details.
    fn from(manifest: Sector) -> Self {
        Self { manifest }
    }
}

impl Serialize for Header {
    type Buffer = [u8; size_of::<Self>()];

    fn size(&self) -> Result<NonZeroU64, number::Error> {
        { size_of::<Self>() as u64 }.try_into().map_err(number::Error::from)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], number::Error> {
        buf.serialize_push(&self.manifest)
    }

    fn serialize(&self) -> Result<Self::Buffer, number::Error> {
        [0u8; size_of::<Self>()].serialize_push(self)
    }
}

impl<'de> Deserialize<'de> for Header {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        const N: usize = size_of_val(&MAGIC) + size_of_val(&VERSION);
        *src = src
            .split_first_chunk::<N>()
            .ok_or_else(|| Error::Truncated { expected: HEADER, actual: src.len() })
            .and_then(|data| match data.0.starts_with(&MAGIC) {
                true => Ok(data),
                false => Error::Magic.into(),
            })
            .and_then(|data| {
                match data.0.last().ok_or_else(|| Error::Truncated {
                    expected: size_of::<u8>(),
                    actual: data.0.len(),
                })? {
                    &VERSION => Ok(data.1),
                    &other => Error::Version(other).into(),
                }
            })?;
        let header = Sector {
            offset: u64::deserialize(src)?,
            size: NonZeroU64::deserialize(src)?,
        }
        .into();
        Ok(header)
    }
}

/// An exclusive owned file handle for an open [msca](crate) dataset.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug)]
pub(crate) struct File {
    /// todo → field doc comment
    pub file: fs::File,
    /// todo → field doc comment
    pub header: Header,
    /// todo → field doc comment
    pub manifest: Manifest,
    /// todo → field doc comment
    pub path: PathBuf,
}

impl File {
    /// Create a new [msca](crate) file with read and write permissions at the specified
    /// [`path`](P).
    ///
    /// The file is initialised in a valid empty state with a default [`Manifest`] segment and no
    /// data segments or [`Metadata`][1]. The manifest [`Segment`] is written immediately after the
    /// file [`Header`].
    ///
    /// ```text
    /// [Header] [Manifest]
    ///         ↑ manifest.offset
    /// ```
    ///
    /// Implementors must ensure that the provided [`path`](P) remains valid and accessible for the
    /// entire duration of the operation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the underlying system call fails. This can occur for a variety of
    /// reasons, including:
    ///
    /// - A file already exists at the specified [`path`](P)
    /// - The current process lacks read and write permissions
    ///
    /// Returns [`Error::Zero`] if a `u64` overflow occurs while calculating `size` or `offset` for
    /// the relevant file regions.
    // [1]: todo → link to metadata struct or feature documentation
    pub(crate) async fn create<P>(path: P) -> Result<Self, Error>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .truncate(false)
            .open(&path)
            .await?;
        file.write_all(&MAGIC).await?;
        file.write_all(&[VERSION]).await?;
        // NOTE: manifest is written directly after the file header (no immutable segments)
        let manifest = Manifest::default();
        let header: Header = manifest.write(&mut file, HEADER as u64).await?.into();
        Header::SECTOR.seek_to_start(&mut file).await?;
        file.write_all(&header.serialize()?).await?;
        file.write_all(&[u8::MIN; ALIGN]).await?;
        file.flush().await?;
        Ok(Self { file, header, manifest, path })
    }

    /// Open an existing [msca](crate) file with read and write permissions at the specified
    /// [`path`](P).
    ///
    /// The [magic bytes](MAGIC) and [version number](VERSION) are validated immediately on open.
    /// Implementors must ensure that the provided `path` remains valid and accessible for the
    /// entire duration of the operation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the underlying system call fails. This can occur for a variety of
    /// reasons, including:
    ///
    /// - A file already exists at the specified [`path`](P)
    /// - The current process lacks read and write permissions
    /// - Unexpected `EOF` while parsing the [`Header`] or [`Manifest`]
    ///
    /// Returns [`Error::Zero`] if a `u64` overflow occurs while calculating `size` or `offset` for
    /// the relevant file regions.
    pub(crate) async fn open<P>(path: P) -> Result<Self, Error>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(false) // Explicitly disallow file creation. Use File::create instead.
            .truncate(false)
            .open(&path)
            .await?;
        let header = Header::from_file(&mut file).await?;
        let manifest = Manifest::from_file(&mut file, header.manifest).await?;
        Ok(Self { file, header, manifest, path })
    }

    /// Create a read-only [memory map](Mmap) backed by the [msca](crate) file.
    ///
    /// ### Errors
    ///
    /// [`Error::Zero`] if the [`Header`](HEADER) size exceeds [`u64::MAX`].
    // todo → Static assert HEADER size as u64, remove try_into runtime checks, use faster unchecked fn.
    ///
    /// [`Error::Io`] if the underlying system call fails. This can occur for a variety of reasons,
    /// such as the file is no longer accessible, or the platform does not support memory mapping.
    ///
    /// ### ⚠️ Safety
    ///
    /// This function is marked as [unsafe][1] because of the potential for undefined behaviour if
    /// the underlying file region is subsequently modified, in or out of process. Implementers are
    /// strongly advised to take appropriate precautions and ensure the mapped region is not
    /// accessed or modified concurrently in a way that causes undefined behaviour.
    ///
    /// [`Segments`](Segment) are immutable once written. The [`Mmap`] is tightly scoped to reduce
    /// the risk of undefined behaviour:
    ///
    /// - [`offset`](MmapOptions::offset) excludes the mutable [`Header`]
    /// - [`length`](MmapOptions::len) excludes the mutable [`Manifest`]
    /// - Only the immutable segment region is mapped
    ///
    /// The [`Arc`][2]`<`[`Mmap`](Mmap)`>` is updated after each [write-cycle](self) to include the
    /// newly appended segment. New readers [`Clone`] the [`Arc`][2] to increment its reference
    /// count. Existing mmaps are released only when their reference count drops to zero; in-flight
    /// reader mmaps remain valid because existing segments are never altered. Buffer [`Sector`]
    /// offsets are recorded relative to the immutable segment region and index the [`Mmap`]
    /// directly; no runtime offset arithmetic.
    ///
    /// Refer to the [memmap](memmap2) crate for more details.
    ///
    /// [1]: https://doc.rust-lang.org/book/ch20-01-unsafe-rust.html
    /// [2]: std::sync::Arc
    /// [3]: crate::dataset::Dataset
    pub(crate) unsafe fn mmap(&self) -> Result<Mmap, Error> {
        let offset: u64 = HEADER.try_into()?;
        let length: usize = { self.header.manifest.offset - offset }.try_into()?;
        // SAFETY: Undefined behaviour if mapped region is modified (refer to mmap documentation)
        unsafe { MmapOptions::new().offset(offset).len(length).map(&self.file).map_err(Error::Io) }
    }

    /// Append a new [`Segment`] to the file and return the written [`Sector`].
    ///
    /// ### Errors
    ///
    /// - [`Error::Manifest`] if an underlying manifest entry [collision][1] occurs.
    /// - [`Error::Io`] if any underlying [seek][2] or [write][3] operations fail.
    /// - [`Error::Number`] if the segment `size` or `offset` overflow `u64`.
    ///
    /// Refer to the [write-cycle](self) documentation for more details.
    ///
    /// [1]: manifest::Error::Collision
    /// [2]: Sector::seek_to_start
    /// [3]: Segment::write
    pub(crate) async fn write<S>(&mut self, seg: S) -> Result<Sector, Error>
    where
        S: Register + Segment,
        Error: From<S::Error>,
    {
        // Phase 1: Reserve the manifest entry; collisions are eagerly rejected w/o file IO
        let entry = self.manifest.entry(&seg)?;
        // Phase 2: Append the new segment, overwriting the on-disk manifest
        let sec = seg.write(&mut self.file, self.header.manifest.offset).await?;
        // Phase 3: Register the segment to the in-memory manifest
        let next = seg.register(&sec, entry)?.next().ok_or(number::Error::Zero)?.get();
        // Phase 4: Append the new manifest directly after the new segment
        self.header.manifest = self.manifest.write(&mut self.file, next).await?;
        // Phase 5: Overwrite the file header manifest sector
        Header::SECTOR.seek_to_start(&mut self.file).await?;
        self.file.write_all(&self.header.serialize()?).await?;
        self.file.flush().await?;
        Ok(sec)
    }
}

/* ------------------------------------------------------------------------------ Specific Error */

/// Errors returned by [`File`](File) [`IO`](self).
///
/// Enum variants cover various granular error cases that may arise when working with the underlying
/// file. Users should consider handling errors explicitly wherever possible to provide meaningful
/// error messages and recovery actions.
///
/// ### Implementation
///
/// This enum is `#[non_exhaustive]` meaning additional variants may be added in future versions.
/// Implementers are advised to include a wildcard arm `_` to account for potential additions.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug)]
#[non_exhaustive] // To accommodate potential future error cases.
pub enum Error {
    /// The computed [`XXH3`][1] checksum does not match the on-disk checksum suffix.
    ///
    /// [1]: https://xxhash.com
    Checksum,
    /// Underlying [`decode::Error`][1] from a [`CBOR`](minicbor) decode operation.
    ///
    /// [1]: minicbor::decode::Error
    Decode(minicbor::decode::Error),
    /// Underlying [`std::io::Error`] from the file backing the [`Dataset`](crate::Dataset).
    Io(std::io::Error),
    /// File magic bytes did not match the expected [msca](MAGIC) signature.
    Magic,
    /// Underlying [`manifest::Error`] from [`Segment`] registration or retrieval.
    Manifest(manifest::Error),
    /// Underlying [`number::Error`] from a numerical operation or conversion.
    Number(number::Error),
    /// Underlying [`schema::Error`] from schema registration or validation.
    Schema(schema::Error),
    /// Underlying [`segment::Error`] from [`Segment`] operations such as [serialisation](Serialize)
    /// or [deserialisation](Deserialize).
    Segment(segment::Error),
    /// Underlying [`TryFromSliceError`] while parsing a slice into a fixed-size array.
    Slice(TryFromSliceError),
    /// A read operation attempted to access bytes beyond the end of the input slice.
    Truncated {
        /// Expected length of the input slice.
        expected: usize,
        /// Actual length of the input slice.
        actual: usize,
    },
    /// The specified `u32` is not a valid [Unicode scalar value][1].
    ///
    /// [1]: https://www.unicode.org/glossary/#unicode_scalar_value
    Utf8,
    /// File version number is not recognised by this build of [msca](crate).
    Version(u8),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Checksum => write!(f, "Checksum error"),
            Self::Decode(e) => write!(f, "CBOR decode error → {e}"),
            Self::Io(e) => write!(f, "File IO error → {e}"),
            Self::Magic => f.write_str("File is not a valid MSCA dataset"),
            Self::Manifest(e) => write!(f, "Manifest error → {e}"),
            Self::Number(e) => write!(f, "Number error → {e}"),
            Self::Schema(e) => write!(f, "Schema error → {e}"),
            Self::Segment(e) => write!(f, "Segment error → {e}"),
            Self::Slice(e) => write!(f, "Try from slice error → {e}"),
            Self::Truncated { .. } => write!(f, "Read was truncated → {self:?}"),
            Self::Utf8 => write!(f, "Invalid UTF8 scalar value"),
            Self::Version(v) => write!(f, "Unrecognised MSCA version → {v}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<TryFromSliceError> for Error {
    fn from(e: TryFromSliceError) -> Self {
        Self::Slice(e)
    }
}

impl From<TryFromIntError> for Error {
    fn from(e: TryFromIntError) -> Self {
        number::Error::from(e).into()
    }
}

impl From<Utf8Error> for Error {
    #[allow(unused_variables)]
    fn from(e: Utf8Error) -> Self {
        Self::Utf8
    }
}

impl From<minicbor::decode::Error> for Error {
    fn from(e: minicbor::decode::Error) -> Self {
        Self::Decode(e)
    }
}

impl From<number::Error> for Error {
    fn from(e: number::Error) -> Self {
        Self::Number(e)
    }
}

impl From<manifest::Error> for Error {
    fn from(e: manifest::Error) -> Self {
        Self::Manifest(e)
    }
}

impl From<schema::Error> for Error {
    fn from(e: schema::Error) -> Self {
        match e {
            schema::Error::Number(err) => Self::Number(err),
            other => Self::Schema(other),
        }
    }
}

impl From<segment::Error> for Error {
    fn from(e: segment::Error) -> Self {
        Self::Segment(e)
    }
}

impl From<Infallible> for Error {
    fn from(value: Infallible) -> Self {
        match value {}
    }
}

//noinspection DuplicatedCode → Conversion is implemented for error types across different modules.
impl<T, E> From<Error> for Result<T, E>
where
    E: From<Error>,
{
    fn from(error: Error) -> Self {
        Err(E::from(error))
    }
}

/* --------------------------------------------------------------------- Buffer Trait Definition */

/// A **buffer** that can hold the serialized byte representation of a value.
///
/// Blanket implementations are provided for stack-allocated byte arrays and heap-allocated byte
/// vectors. This design defines a fixed-size buffer for types with a known size at compile time,
/// while facilitating dynamic buffer sizing for types that require heap allocation.
pub trait Buffer: AsRef<[u8]> + AsMut<[u8]> + Into<Vec<u8>> {
    /// [`Serialize`] the provided [`item`](I) and append into [`self`](Buffer).
    ///
    /// Writing always begins at the **start** of the buffer; chained calls overwrite. Chain
    /// sequential writes through [`Serialize::serialize_into`], which returns the advanced slice.
    fn serialize_push<I: Serialize>(self, item: &I) -> Result<Self, number::Error>;
}

/* ----------------------------------------------------------------- Buffer Trait Implementation */

impl Buffer for &mut [u8] {
    fn serialize_push<I: Serialize>(self, item: &I) -> Result<Self, number::Error> {
        item.serialize_into(self)
    }
}

impl<const N: usize> Buffer for [u8; N] {
    fn serialize_push<I: Serialize>(mut self, item: &I) -> Result<Self, number::Error> {
        item.serialize_into(&mut self)?;
        Ok(self)
    }
}

impl Buffer for Vec<u8> {
    fn serialize_push<I: Serialize>(self, item: &I) -> Result<Self, number::Error> {
        item.extend(self)
    }
}

/* ---------------------------------------------------------------- Deserialize Trait Definition */

/// A **type** that can be deserialized from a [byte source][1].
///
/// ### Lifetimes
///
/// The `'de` lifetime binds the [`Self::Ok`] output type to the source bytes, enabling zero-copy
/// borrows for [unsized][2] types without an intermediate owned copy. [`Sized`] types return an
/// owned [`Self`] directly without lifetime constraints.
///
/// [1]: https://doc.rust-lang.org/std/primitive.slice.html
/// [2]: https://doc.rust-lang.org/reference/dynamically-sized-types.html
pub trait Deserialize<'de> {
    /// Output following successful deserialisation.
    ///
    /// [`Sized`] types return [`Self`] directly; [unsized][1] types return `&'de Self`.
    ///
    /// [1]: https://doc.rust-lang.org/reference/dynamically-sized-types.html
    type Ok;

    /// Remove the required number of bytes from the provided [source][1] and deserialize into
    /// one instance of [`Self::Ok`]; advancing the source **in-situ** without an intermediate copy
    /// by the number of bytes read.
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Truncated`] if `src` contains fewer than the requested number of bytes.
    ///
    /// [1]: https://doc.rust-lang.org/std/primitive.slice.html
    fn deserialize(src: &mut &'de [u8]) -> Result<Self::Ok, Error>;
}

/* ------------------------------------------------------------ Deserialize Trait Implementation */

impl<'de, I> Deserialize<'de> for &'de I
where
    I: Deserialize<'de, Ok = &'de I> + ?Sized,
{
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self::Ok, Error> {
        I::deserialize(src)
    }
}

impl<'de> Deserialize<'de> for u8 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        src.split_first()
            .ok_or_else(|| Error::Truncated {
                expected: size_of::<Self>(),
                actual: src.len(),
            })
            .map(|data| {
                *src = data.1;
                Self::from(*data.0)
            })
    }
}

impl<'de> Deserialize<'de> for u16 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        src.split_first_chunk()
            .ok_or_else(|| Error::Truncated {
                expected: size_of::<Self>(),
                actual: src.len(),
            })
            .map(|data| {
                *src = data.1;
                Self::from_le_bytes(*data.0)
            })
    }
}

impl<'de> Deserialize<'de> for u32 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        src.split_first_chunk()
            .ok_or_else(|| Error::Truncated {
                expected: size_of::<Self>(),
                actual: src.len(),
            })
            .map(|data| {
                *src = data.1;
                Self::from_le_bytes(*data.0)
            })
    }
}

impl<'de> Deserialize<'de> for u64 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        src.split_first_chunk()
            .ok_or_else(|| Error::Truncated {
                expected: size_of::<Self>(),
                actual: src.len(),
            })
            .map(|data| {
                *src = data.1;
                Self::from_le_bytes(*data.0)
            })
    }
}

impl<'de> Deserialize<'de> for u128 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        src.split_first_chunk()
            .ok_or_else(|| Error::Truncated {
                expected: size_of::<Self>(),
                actual: src.len(),
            })
            .map(|data| {
                *src = data.1;
                Self::from_le_bytes(*data.0)
            })
    }
}

impl<'de> Deserialize<'de> for num::NonZeroU8 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        u8::deserialize(src)?.try_into().map_err(Into::into)
    }
}

impl<'de> Deserialize<'de> for num::NonZeroU16 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        u16::deserialize(src)?.try_into().map_err(Into::into)
    }
}

impl<'de> Deserialize<'de> for num::NonZeroU32 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        u32::deserialize(src)?.try_into().map_err(Into::into)
    }
}

impl<'de> Deserialize<'de> for NonZeroU64 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        u64::deserialize(src)?.try_into().map_err(Into::into)
    }
}

impl<'de> Deserialize<'de> for num::NonZeroU128 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        u128::deserialize(src)?.try_into().map_err(Into::into)
    }
}

impl<'de> Deserialize<'de> for i8 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        src.split_first_chunk()
            .ok_or_else(|| Error::Truncated {
                expected: size_of::<Self>(),
                actual: src.len(),
            })
            .map(|data| {
                *src = data.1;
                Self::from_le_bytes(*data.0)
            })
    }
}

impl<'de> Deserialize<'de> for i16 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        src.split_first_chunk()
            .ok_or_else(|| Error::Truncated {
                expected: size_of::<Self>(),
                actual: src.len(),
            })
            .map(|data| {
                *src = data.1;
                Self::from_le_bytes(*data.0)
            })
    }
}

impl<'de> Deserialize<'de> for i32 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        src.split_first_chunk()
            .ok_or_else(|| Error::Truncated {
                expected: size_of::<Self>(),
                actual: src.len(),
            })
            .map(|data| {
                *src = data.1;
                Self::from_le_bytes(*data.0)
            })
    }
}

impl<'de> Deserialize<'de> for i64 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        src.split_first_chunk()
            .ok_or_else(|| Error::Truncated {
                expected: size_of::<Self>(),
                actual: src.len(),
            })
            .map(|data| {
                *src = data.1;
                Self::from_le_bytes(*data.0)
            })
    }
}

impl<'de> Deserialize<'de> for i128 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        src.split_first_chunk()
            .ok_or_else(|| Error::Truncated {
                expected: size_of::<Self>(),
                actual: src.len(),
            })
            .map(|data| {
                *src = data.1;
                Self::from_le_bytes(*data.0)
            })
    }
}

impl<'de> Deserialize<'de> for num::NonZeroI8 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        i8::deserialize(src)?.try_into().map_err(Into::into)
    }
}

impl<'de> Deserialize<'de> for num::NonZeroI16 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        i16::deserialize(src)?.try_into().map_err(Into::into)
    }
}

impl<'de> Deserialize<'de> for num::NonZeroI32 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        i32::deserialize(src)?.try_into().map_err(Into::into)
    }
}

impl<'de> Deserialize<'de> for num::NonZeroI64 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        i64::deserialize(src)?.try_into().map_err(Into::into)
    }
}

impl<'de> Deserialize<'de> for num::NonZeroI128 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        i128::deserialize(src)?.try_into().map_err(Into::into)
    }
}

impl<'de> Deserialize<'de> for f32 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        src.split_first_chunk()
            .ok_or_else(|| Error::Truncated {
                expected: size_of::<Self>(),
                actual: src.len(),
            })
            .map(|data| {
                *src = data.1;
                Self::from_le_bytes(*data.0)
            })
    }
}

impl<'de> Deserialize<'de> for f64 {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        src.split_first_chunk()
            .ok_or_else(|| Error::Truncated {
                expected: size_of::<Self>(),
                actual: src.len(),
            })
            .map(|data| {
                *src = data.1;
                Self::from_le_bytes(*data.0)
            })
    }
}

impl<'de> Deserialize<'de> for char {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        let utf8 = u32::deserialize(src)?;
        char::from_u32(utf8).ok_or(Error::Utf8)
    }
}

impl<'de> Deserialize<'de> for String {
    type Ok = Self;

    /// Consumes the provided source into one UTF8 [`String`].
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Utf8`] if the source bytes are not valid UTF8.
    ///
    /// Refer to the [trait documentation](Deserialize) for more information.
    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        // NOTE: consumes the entire source; the byte slice must be sized externally
        <&[u8]>::deserialize(src).map(str::from_utf8)?.map(str::to_owned).map_err(Error::from)
    }
}

impl<'de> Deserialize<'de> for Option<num::NonZeroU8> {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        // NOTE: serialize trait encodes none using the u8::MIN niche
        u8::deserialize(src).map(num::NonZeroU8::new)
    }
}

impl<'de> Deserialize<'de> for Option<num::NonZeroU16> {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        // NOTE: serialize trait encodes none using the u16::MIN niche
        u16::deserialize(src).map(num::NonZeroU16::new)
    }
}

impl<'de> Deserialize<'de> for Option<num::NonZeroU32> {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        // NOTE: serialize trait encodes none using the u32::MIN niche
        u32::deserialize(src).map(num::NonZeroU32::new)
    }
}

impl<'de> Deserialize<'de> for Option<NonZeroU64> {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        // NOTE: serialize trait encodes none using the u64::MIN niche
        u64::deserialize(src).map(NonZeroU64::new)
    }
}

impl<'de> Deserialize<'de> for Option<num::NonZeroU128> {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        // NOTE: serialize trait encodes none using the u128::MIN niche
        u128::deserialize(src).map(num::NonZeroU128::new)
    }
}

impl<'de> Deserialize<'de> for Option<num::NonZeroI8> {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        // NOTE: serialize trait encodes none using the i8::MIN niche
        i8::deserialize(src).map(num::NonZeroI8::new)
    }
}

impl<'de> Deserialize<'de> for Option<num::NonZeroI16> {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        // NOTE: serialize trait encodes none using the i16::MIN niche
        i16::deserialize(src).map(num::NonZeroI16::new)
    }
}

impl<'de> Deserialize<'de> for Option<num::NonZeroI32> {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        // NOTE: serialize trait encodes none using the i32::MIN niche
        i32::deserialize(src).map(num::NonZeroI32::new)
    }
}

impl<'de> Deserialize<'de> for Option<num::NonZeroI64> {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        // NOTE: serialize trait encodes none using the i64::MIN niche
        i64::deserialize(src).map(num::NonZeroI64::new)
    }
}

impl<'de> Deserialize<'de> for Option<num::NonZeroI128> {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        // NOTE: serialize trait encodes none using the i128::MIN niche
        i128::deserialize(src).map(num::NonZeroI128::new)
    }
}

impl<'de> Deserialize<'de> for Option<char> {
    type Ok = Self;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self, Error> {
        // NOTE: serialize trait encodes none using the u32::MAX niche
        match u32::deserialize(src)? {
            u32::MAX => Ok(None),
            utf8 => char::from_u32(utf8).map(Some).ok_or(Error::Utf8),
        }
    }
}
impl<'de> Deserialize<'de> for [u8] {
    type Ok = &'de [u8];

    fn deserialize(src: &mut &'de [u8]) -> Result<Self::Ok, Error> {
        // NOTE: consumes the entire source and replaces in situ with an empty byte slice
        let out = mem::take(src);
        Ok(out)
    }
}

impl<'de> Deserialize<'de> for BitSlice<u8, Lsb0> {
    type Ok = &'de BitSlice<u8, Lsb0>;

    fn deserialize(src: &mut &'de [u8]) -> Result<Self::Ok, Error> {
        let bits = mem::take(src).view_bits();
        Ok(bits)
    }
}

/* --------------------------------------------------------------- Deserializer Trait Definition */

/// A **source** that can be deserialized into a [supported data type](Deserialize).
///
/// ### Lifetimes
///
/// The `'de` lifetime binds each deserialized target to the underlying [data source][1] lifetime,
/// enabling **zero-copy** borrows for [unsized][2] types.
///
/// [1]: https://doc.rust-lang.org/std/primitive.slice.html
/// [2]: https://doc.rust-lang.org/reference/dynamically-sized-types.html
pub trait Deserializer<'de> {
    /// Consume the required number of bytes from [`Self`] and [`Deserialize`] into one owned
    /// instance of the target type [`I`]; advancing the underlying [data source][1] **in-situ**
    /// without an intermediate copy.
    ///
    /// [1]: https://doc.rust-lang.org/std/primitive.slice.html
    #[rustfmt::skip] // single line where clause improves readability
    fn deserialize_into<I>(&mut self) -> Result<I, Error> where I: Deserialize<'de, Ok = I>;
}

/* ----------------------------------------------------------- Deserializer Trait Implementation */

impl<'de> Deserializer<'de> for &'de [u8] {
    fn deserialize_into<I>(&mut self) -> Result<I, Error>
    where
        I: Deserialize<'de, Ok = I>,
    {
        I::deserialize(self)
    }
}

impl<'de> Deserializer<'de> for SizedBuf<&'de [u8]> {
    fn deserialize_into<I>(&mut self) -> Result<I, Error>
    where
        I: Deserialize<'de, Ok = I>,
    {
        self.0.deserialize_into()
    }
}

/* ------------------------------------------------------------------- Checksum Trait Definition */

/// A [`Segment`] with a `u64` [`XXH3`][2] checksum suffix to [`verify`](Self::verify) the validity
/// of every preceding byte in the [slice].
///
/// [1]: https://doc.rust-lang.org/std/primitive.slice.html
/// [2]: https://xxhash.com
pub(crate) trait Checksum {
    /// Calculate the [`XXH3`][1] checksum and [`Serialize`] into the provided buffer.
    ///
    /// [1]: https://xxhash.com
    fn checksum(buf: &mut [u8]) -> Result<&mut [u8], number::Error> {
        const N: usize = size_of::<u64>();
        buf.split_last_chunk_mut::<N>()
            .map(|data| xxh3_64(data.0).serialize_into(data.1))
            .ok_or(number::Error::Zero)
            .flatten()?;
        Ok(buf)
    }

    /// Split the [`XXH3`][1] checksum suffix from the provided byte [slice][2] and verify against a
    /// calculated checksum from the preceding bytes.
    ///
    /// ### Errors
    ///
    /// - [`Error::Truncated`] if the buffer is shorter than the `u64` checksum.
    /// - [`Error::Checksum`] if the recorded checksum does not match the computed checksum.
    ///
    /// [1]: https://xxhash.com
    /// [2]: https://doc.rust-lang.org/std/primitive.slice.html
    fn verify(buf: &[u8]) -> Result<&[u8], Error> {
        buf.split_last_chunk()
            .map(|b| match u64::from_le_bytes(*b.1) == xxh3_64(b.0) {
                true => Ok(b.0),
                false => Err(Error::Checksum),
            })
            .ok_or_else(|| Error::Truncated {
                expected: size_of::<u64>(),
                actual: buf.len(),
            })
            .flatten()
    }
}

/* ------------------------------------------------------------------- Register Trait Definition */

/// An on-disk **data structure** that can be recorded to the file [`Manifest`].
pub(crate) trait Register {
    /// The [`Error`](std::error::Error) type returned by fallible [`Manifest`] operations.
    type Error: Into<Error>;

    /// The **reserved** entry type returned by [`entry`][1] and populated by [`register`][2].
    ///
    /// [1]: Register::entry
    /// [2]: Register::register
    type Entry<'m>;

    /// Reserve a vacant [`entry`](Self::Entry) in the [`Manifest`] before file [`IO`][1] occurs.
    ///
    /// The reserved entry is consumed by [`register`](Register::register); meaning manifest lookup
    /// is performed exactly once.
    ///
    /// ### Errors
    ///
    /// Returns [`Self::Error`] if the entry cannot be reserved. Refer to each implementation for
    /// specific error cases.
    ///
    /// [1]: File::write
    fn entry<'m>(&self, m: &'m mut Manifest) -> Result<Self::Entry<'m>, Self::Error>;

    /// Consume `self` and populate the reserved [`entry`](Self::entry) with a **descriptor**
    /// generated from the authoritive written [`Sector`], enforcing the [reserve](Self::entry) →
    /// [write](File::write) → [populate](Self::register) data flow.
    ///
    /// Returns the provided [`Sector`] for subsequent function chaining.
    fn register<'s, 'm>(self, s: &'s Sector, e: Self::Entry<'m>)
    -> Result<&'s Sector, Self::Error>;
}

/* --------------------------------------------------------------------------------------- Tests */

#[cfg(test)]
mod tests {
    use super::*;

    /* ------------------------------------------------------------------------------ Unit Tests */

    /// [`Sector`] orders by offset, so a later sector compares greater than an earlier one.
    #[test]
    fn sector_orders_by_offset() {
        let hi = Sector::new(200, 16).expect("Sector::new failed for hi");
        let lo = Sector::new(100, 16).expect("Sector::new failed for lo");
        assert!(hi > lo);
        assert!(lo < hi);
    }

    /// Two sectors at one offset are unequal when their sizes differ.
    #[test]
    fn sector_distinguishes_size() {
        let short = Sector::new(100, 16).expect("Sector::new failed for short");
        let long = Sector::new(100, 32).expect("Sector::new failed for long");
        assert_ne!(short, long);
    }

    /// [`Sector`] is [`Copy`], so a bind duplicates rather than moves.
    #[test]
    fn sector_is_copy() {
        let a = Sector::new(10, 5).expect("Sector::new failed");
        let b = a;
        assert_eq!(a, b);
    }

    /// [`Sector::new`] rejects a zero `length`; every sector must describe at least one byte.
    #[test]
    fn sector_rejects_zero_length() {
        Sector::new(100, 0).expect_err("Zero-length sector accepted");
    }

    /// [`Sector::next`] returns [`None`] when the trailing offset overflows `u64`.
    #[test]
    fn sector_next_reports_overflow() {
        let sector = Sector::new(u64::MAX, 1u64).expect("Sector::new failed");
        assert!(sector.next().is_none());
    }

    /// File [`HEADER`] is rounded up ↑ to the next 64-bit alignment boundary.
    #[test]
    fn header_aligns_to_boundary() {
        assert_eq!(HEADER % 8, 0);
        assert!(ALIGN < 8);
    }

    /// [`[u8]`][1] deserialization is a no-op yielding the whole [source][1] as the payload; leaf
    /// buffers are framed externally by their [`Sector`], so the entire slice is the value and the
    /// source is fully consumed.
    ///
    /// [1]: https://doc.rust-lang.org/std/primitive.slice.html
    #[test]
    fn deserialize_slice_takes_whole_source() {
        let data = [1u8, 2, 3];
        let mut src = data.as_slice();
        let payload = <[u8]>::deserialize(&mut src).expect("Deserialize failed");
        assert_eq!(payload, &[1, 2, 3]);
        assert!(src.is_empty());
    }

    /// [`BitSlice`] deserialization views the whole [source][1] as bits with no length header.
    ///
    /// [1]: https://doc.rust-lang.org/std/primitive.slice.html
    #[test]
    fn deserialize_bit_slice_views_whole_source() {
        let data = [0b0000_0101u8];
        let mut src = data.as_slice();
        let bits = BitSlice::<u8, Lsb0>::deserialize(&mut src).expect("Deserialize failed");
        assert!(bits[0] && !bits[1] && bits[2]);
        assert!(src.is_empty());
    }

    /// [`SizedBuf`] serialization writes the payload behind a length prefix recording the exact
    /// payload size, padded to the next 64-bit boundary; deserialization recovers the payload and
    /// fully consumes the source.
    #[test]
    fn sized_buf_round_trips() {
        let expect = [3, 0, 0, 0, 0, 0, 0, 0, b'a', b'b', b'c', 0, 0, 0, 0, 0];
        let bytes = SizedBuf::new(*b"abc").serialize().expect("Serialize failed");
        let mut src = bytes.as_slice();
        let region = SizedBuf::deserialize(&mut src).expect("Deserialize failed");
        assert_eq!(bytes, expect); // prefix + payload + alignment
        assert_eq!(region.0, b"abc");
        assert!(src.is_empty());
    }

    /// [`SizedBuf::size`] predicts exactly the number of bytes written by
    /// [`SizedBuf::serialize`]: prefix plus payload plus alignment padding.
    #[test]
    fn sized_buf_size_matches_bytes_written() {
        let short = SizedBuf::new(*b"abc");
        let exact = SizedBuf::new(u64::MAX.to_le_bytes());
        assert_eq!(short.size().expect("Size failed").get(), 16);
        assert_eq!(short.serialize().expect("Serialize failed").len(), 16);
        assert_eq!(exact.size().expect("Size failed").get(), 16);
        assert_eq!(exact.serialize().expect("Serialize failed").len(), 16);
    }

    /// [`SizedBuf::deserialize`] consumes the zero-filled padding after each payload, landing each
    /// sequential read on the next 64-bit alignment boundary.
    #[test]
    fn sized_buf_deserialize_skips_padding() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&3u64.to_le_bytes());
        buf.extend_from_slice(b"abc\0\0\0\0\0"); // payload padded to the next boundary
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.extend_from_slice(b"z\0\0\0\0\0\0\0"); // writers pad every region, including the last
        let mut src = buf.as_slice();
        let first = SizedBuf::deserialize(&mut src).expect("First deserialize failed");
        let second = SizedBuf::deserialize(&mut src).expect("Second deserialize failed");
        assert_eq!(first.0, b"abc");
        assert_eq!(second.0, b"z");
        assert!(src.is_empty());
    }

    /// [`SizedBuf`] rejects an empty payload; zero-length regions are never written because
    /// writers omit empty regions entirely.
    #[test]
    fn sized_buf_rejects_empty_payload() {
        SizedBuf::new(Vec::<u8>::new()).serialize().expect_err("Empty payload serialized");
        SizedBuf::new(Vec::<u8>::new()).size().expect_err("Empty payload sized");
    }

    /// [`SizedBuf::deserialize`] rejects a zero length prefix; empty regions are omitted by
    /// writers so a zero prefix indicates corruption.
    #[test]
    fn sized_buf_rejects_zero_prefix() {
        let bytes = u64::MIN.to_le_bytes();
        SizedBuf::deserialize(&mut bytes.as_slice()).expect_err("Zero prefix accepted");
    }

    /// [`SizedBuf::deserialize`] rejects a source shorter than its recorded length prefix.
    #[test]
    fn sized_buf_rejects_truncated_source() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&10u64.to_le_bytes());
        buf.extend_from_slice(b"abc"); // three payload bytes; the prefix records ten
        SizedBuf::deserialize(&mut buf.as_slice()).expect_err("Truncated source accepted");
    }

    /// The blanket [`Serialize`] implementation for references delegates to the referenced item;
    /// a borrowed payload frames identically to its owned counterpart.
    #[test]
    fn sized_buf_reference_delegates_to_owned() {
        let owned = SizedBuf::new(*b"abc").serialize().expect("Owned serialize failed");
        let borrowed = SizedBuf::new(&*b"abc").serialize().expect("Borrowed serialize failed");
        assert_eq!(owned, borrowed);
    }
}
