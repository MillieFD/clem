//! Domain-agnostic high-throughput storage for n-dimensional analytical data.
//!
//! `clem` implements the columnar storage format described in `FORMAT.md` of the source
//! repository. Files are organised as a sequence of self-describing segments followed by a
//! manifest, supporting append-heavy ingestion and lazy random-access reads via `mmap`.
//!
//! The crate-level [`Sector`] type is a streamable byte range over a memory-mapped file. The
//! [`NonZeroUnsigned`] marker trait enumerates the integer widths approved for storing buffer
//! offsets. The [`Error`] enum is the single error returned from every fallible API.

mod dataset;
mod dictionary;
mod io;
mod manifest;
mod query;
mod schema;
mod segment;
mod stream;
mod substream;

/* --------------------------------------------------------------------------- Private Imports */

use std::cmp::Ordering;
use std::fmt;
use std::num::{NonZeroU8, NonZeroU16, NonZeroU32, NonZeroU64, NonZeroU128};
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use memmap2::Mmap;
use smol::stream::Stream;

/* ---------------------------------------------------------------------------- Public Exports */

// Re-exports from `dataset`, `stream`, `substream`, `query`, and `dictionary` are added
// in subsequent implementation phases.

/// A streamable byte range within a `clem` file.
///
/// [`Sector`] couples a `(offset, length)` pointer with an `Arc`-shared [`Mmap`] handle to the
/// underlying file. It implements [`Stream`] over `u8`, so callers can consume bytes
/// asynchronously inside a `smol`-based pipeline.
///
/// Stream consumption advances an internal cursor; cloning a sector before any bytes have been
/// consumed yields an independent cursor sharing the same `Arc<Mmap>`. Cloning a partially
/// consumed sector preserves the cursor position, mirroring [`std::io::Cursor::clone`].
///
/// Equality compares only `(offset, length)` per spec §2.6 — two sectors describing the same
/// byte range are equal regardless of cursor position or which file mapping backs them.
/// Ordering is descending by `offset`: a sector closer to EOF compares "less than" one earlier
/// in the file.
pub struct Sector {
    /// Byte offset to the start of the sector within the file.
    pub offset: u64,
    /// Length of the sector in bytes.
    pub length: usize,
    /// Shared handle to the memory-mapped file backing this sector.
    mmap: Arc<Mmap>,
    /// Read cursor advanced by the [`Stream`] implementation.
    pos: usize,
}

impl Sector {
    /// Constructs a new [`Sector`] with the cursor initialised to the start of the range.
    #[allow(dead_code, reason = "constructor used by later phases and tests")]
    pub(crate) fn new(offset: u64, length: usize, mmap: Arc<Mmap>) -> Self {
        Self {
            offset,
            length,
            mmap,
            pos: 0,
        }
    }

    /// Returns the [`Range`] for slicing a held `mmap` view of the file.
    pub fn range(&self) -> Range<usize> {
        let start = self.offset as usize;
        start..start + self.length
    }

    /// Returns the underlying byte slice without consuming the [`Stream`] cursor.
    pub fn bytes(&self) -> &[u8] {
        &self.mmap.as_ref()[self.range()]
    }
}

/// Errors returned by `clem`.
#[non_exhaustive]
#[derive(Debug)]
pub enum Error {
    /// Underlying [`std::io::Error`] from the file backing the dataset.
    Io(std::io::Error),
    /// CBOR encode or decode failure for a manifest or schema payload.
    Cbor,
    /// File magic bytes did not match the expected `clem` signature.
    Magic,
    /// File version not recognised by this build of `clem`.
    Version(u8),
    /// Unsupported `serde` construct encountered while building a schema.
    Schema(&'static str),
    /// Type does not exactly match the existing on-disk schema.
    SchemaMismatch,
    /// Type cannot project from the existing on-disk schema.
    SchemaProjection,
    /// Referenced schema name not present in the manifest.
    SchemaMissing(String),
    /// Referenced column name not present in the schema.
    Column(String),
    /// Dictionary type collision in the dataset cache.
    DictionaryKind,
    /// Duplicate key on dictionary push.
    DuplicateKey,
    /// Row count exceeded `u64::MAX` in a single segment.
    Count,
    /// Manifest decode failed and rebuilding from segments did not succeed.
    Manifest,
    /// Filter applied to an incompatible column.
    Filter(&'static str),
    /// Join leg type or column mismatch.
    Join(&'static str),
}

mod sealed {
    pub trait Sealed {}
}

/// Marker trait for the unsigned non-zero integer types approved as buffer offsets.
///
/// Implementations exist for [`NonZeroU8`], [`NonZeroU16`], [`NonZeroU32`], [`NonZeroU64`], and
/// [`NonZeroU128`]. The trait is sealed: implementations outside this crate are not permitted.
///
/// Niche optimisation on `NonZero` types lets `Option<Self>` share the same memory layout as
/// `Self`, encoding the absent state by storing the all-zero pattern. The format spec relies on
/// this for the `offsets` array in data segment headers, where omitted columns occupy the niche.
///
/// [`NonZeroU8`]: std::num::NonZeroU8
/// [`NonZeroU16`]: std::num::NonZeroU16
/// [`NonZeroU32`]: std::num::NonZeroU32
/// [`NonZeroU64`]: std::num::NonZeroU64
/// [`NonZeroU128`]: std::num::NonZeroU128
pub trait NonZeroUnsigned: Copy + Ord + sealed::Sealed {
    /// Width in bytes of the underlying integer.
    const SIZE: usize;
    /// Writes the value as little-endian bytes into `dst`.
    fn encode(self, dst: &mut [u8]);
    /// Reads a value from little-endian bytes in `src`. Returns [`None`] if the parsed integer
    /// is zero (the niche state).
    fn decode(src: &[u8]) -> Option<Self>;
}

/* ----------------------------------------------------------------------- Trait Implementations */

impl Clone for Sector {
    fn clone(&self) -> Self {
        Self {
            offset: self.offset,
            length: self.length,
            mmap: Arc::clone(&self.mmap),
            pos: self.pos,
        }
    }
}

impl fmt::Debug for Sector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sector")
            .field("offset", &self.offset)
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Sector {
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset && self.length == other.length
    }
}

impl Eq for Sector {}

impl Ord for Sector {
    fn cmp(&self, other: &Self) -> Ordering {
        other.offset.cmp(&self.offset)
    }
}

impl PartialOrd for Sector {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Stream for Sector {
    type Item = u8;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<u8>> {
        let me = self.get_mut();
        Poll::Ready((me.pos < me.length).then(|| {
            let idx = me.offset as usize + me.pos;
            me.pos += 1;
            me.mmap.as_ref()[idx]
        }))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Cbor => f.write_str("cbor encode or decode failed"),
            Self::Magic => f.write_str("invalid magic bytes"),
            Self::Version(v) => write!(f, "unrecognised version: {v}"),
            Self::Schema(s) => write!(f, "schema build: {s}"),
            Self::SchemaMismatch => f.write_str("schema mismatch"),
            Self::SchemaProjection => f.write_str("schema cannot project"),
            Self::SchemaMissing(s) => write!(f, "schema not found: {s}"),
            Self::Column(c) => write!(f, "column not found: {c}"),
            Self::DictionaryKind => f.write_str("dictionary type collision"),
            Self::DuplicateKey => f.write_str("duplicate dictionary key"),
            Self::Count => f.write_str("row count overflow"),
            Self::Manifest => f.write_str("manifest decode failed"),
            Self::Filter(s) => write!(f, "filter: {s}"),
            Self::Join(s) => write!(f, "join: {s}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

macro_rules! impl_nzu {
    ($($t:ident => $p:ty),* $(,)?) => {
        $(
            impl sealed::Sealed for $t {}
            impl NonZeroUnsigned for $t {
                const SIZE: usize = size_of::<$p>();
                fn encode(self, dst: &mut [u8]) {
                    dst[..Self::SIZE].copy_from_slice(&self.get().to_le_bytes());
                }
                fn decode(src: &[u8]) -> Option<Self> {
                    let mut buf = [0u8; size_of::<$p>()];
                    buf.copy_from_slice(&src[..Self::SIZE]);
                    Self::new(<$p>::from_le_bytes(buf))
                }
            }
        )*
    };
}

impl_nzu! {
    NonZeroU8 => u8,
    NonZeroU16 => u16,
    NonZeroU32 => u32,
    NonZeroU64 => u64,
    NonZeroU128 => u128,
}

/* ------------------------------------------------------------------------------------- Tests */

#[cfg(test)]
mod tests {
    use memmap2::MmapMut;
    use smol::stream::StreamExt;

    use super::*;

    /// Builds a [`Sector`] over an anonymous in-memory mapping containing `data`.
    fn test_sector(offset: u64, length: usize, data: &[u8]) -> Sector {
        // SAFETY: Anonymous mmap of small length cannot fail on supported platforms.
        let mut m = MmapMut::map_anon(data.len().max(1)).expect("Anonymous mmap creation failed");
        m[..data.len()].copy_from_slice(data);
        // SAFETY: Freezing a writable mmap to read-only is infallible after a successful map.
        let mmap = Arc::new(m.make_read_only().expect("Mmap freeze failed"));
        Sector::new(offset, length, mmap)
    }

    #[test]
    fn sector_ord_descending() {
        let a = test_sector(100, 16, &[]);
        let b = test_sector(200, 16, &[]);
        // Closer to EOF (b) is "less than" earlier (a) per spec §2.6.
        assert!(b < a);
        assert!(a > b);
    }

    #[test]
    fn sector_eq_compares_offset_and_length() {
        let a = test_sector(100, 16, &[]);
        let b = test_sector(100, 16, &[]);
        let c = test_sector(100, 32, &[]);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn sector_range_for_slicing() {
        let s = test_sector(10, 5, &[]);
        assert_eq!(s.range(), 10..15);
    }

    #[test]
    fn sector_bytes_returns_full_slice() {
        let s = test_sector(2, 4, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(s.bytes(), &[3, 4, 5, 6]);
    }

    #[test]
    fn sector_streams_bytes_in_order() {
        let s = test_sector(2, 4, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let bytes: Vec<u8> = smol::block_on(s.collect());
        assert_eq!(bytes, vec![3, 4, 5, 6]);
    }

    #[test]
    fn sector_clone_before_consumption_yields_independent_cursors() {
        let original = test_sector(0, 4, &[10, 20, 30, 40]);
        let clone = original.clone();
        let from_orig: Vec<u8> = smol::block_on(original.collect());
        let from_clone: Vec<u8> = smol::block_on(clone.collect());
        assert_eq!(from_orig, vec![10, 20, 30, 40]);
        assert_eq!(from_clone, vec![10, 20, 30, 40]);
    }

    #[test]
    fn sector_clone_preserves_partial_consumption() {
        let mut s = test_sector(0, 4, &[10, 20, 30, 40]);
        let first = smol::block_on(s.next());
        assert_eq!(first, Some(10));
        let resumed: Vec<u8> = smol::block_on(s.clone().collect());
        let remainder: Vec<u8> = smol::block_on(s.collect());
        assert_eq!(resumed, vec![20, 30, 40]);
        assert_eq!(remainder, vec![20, 30, 40]);
    }

    #[test]
    fn nzu_roundtrip() {
        fn check<T: NonZeroUnsigned + fmt::Debug>(v: T) {
            let mut buf = vec![0u8; T::SIZE];
            v.encode(&mut buf);
            assert_eq!(T::decode(&buf), Some(v));
        }
        check(NonZeroU8::MIN);
        check(NonZeroU8::MAX);
        check(NonZeroU16::MIN);
        check(NonZeroU16::MAX);
        check(NonZeroU32::MIN);
        check(NonZeroU32::MAX);
        check(NonZeroU64::MIN);
        check(NonZeroU64::MAX);
        check(NonZeroU128::MIN);
        check(NonZeroU128::MAX);
    }

    #[test]
    fn nzu_decode_zero_is_none() {
        let buf = [0u8; 8];
        assert_eq!(NonZeroU64::decode(&buf), None);
    }

    #[test]
    fn niche_optimisation_holds() {
        assert_eq!(size_of::<Option<NonZeroU64>>(), size_of::<NonZeroU64>());
    }

    #[test]
    fn error_display_includes_io_source() {
        let e: Error = std::io::Error::other("boom").into();
        assert!(format!("{e}").contains("boom"));
    }
}
