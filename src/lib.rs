//! Domain-agnostic high-throughput storage for n-dimensional analytical data.
//!
//! `clem` implements the columnar storage format described in `FORMAT.md` of the source
//! repository. Files are organised as a sequence of self-describing segments followed by a
//! manifest, supporting append-heavy ingestion and lazy random-access reads via `mmap`.
//!
//! The crate-level [`Sector`] type represents a contiguous byte range within the file.
//! The [`NonZeroUnsigned`] marker trait enumerates the integer widths approved for storing
//! buffer offsets. The [`Error`] enum is the single error returned from every fallible API.

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

/* ---------------------------------------------------------------------------- Public Exports */

// Re-exports from `dataset`, `stream`, `substream`, `query`, and `dictionary` are added
// in subsequent implementation phases.

/// A contiguous byte range within a [`clem`](crate) file.
///
/// Represents on-disk data prior to file IO. Passing a small `Sector` reduces overhead
/// compared to passing an owned data buffer. Sectors enforce the immutability of underlying
/// on-disk data; callers must copy into an owned type when mutability is required.
///
/// Ordering is increasing by [`offset`](Sector::offset): a sector closer to EOF compares
/// "greater than" one earlier in the file. Equality compares both [`offset`](Sector::offset)
/// and [`length`](Sector::length).
#[derive(Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Sector {
    /// Byte offset to the start of the sector within the file.
    pub offset: u64,
    /// Length of the sector in bytes.
    pub length: usize,
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
/// Implementations exist for [`NonZeroU8`], [`NonZeroU16`], [`NonZeroU32`], [`NonZeroU64`],
/// and [`NonZeroU128`]. The trait is sealed: implementations outside this crate are not
/// permitted.
///
/// Niche optimisation on `NonZero` types lets `Option<Self>` share the same memory layout
/// as `Self`, encoding the absent state by storing the all-zero pattern. The format spec
/// relies on this for the `offsets` array in data segment headers, where omitted columns
/// occupy the niche.
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
    /// Reads a value from little-endian bytes in `src`. Returns [`None`] if the parsed
    /// integer is zero (the niche state).
    fn decode(src: &[u8]) -> Option<Self>;
}

/* ----------------------------------------------------------------------- Trait Implementations */

impl Ord for Sector {
    fn cmp(&self, other: &Self) -> Ordering {
        self.offset.cmp(&other.offset)
    }
}

impl PartialOrd for Sector {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
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
    use super::*;

    const fn sec(offset: u64, length: usize) -> Sector {
        Sector { offset, length }
    }

    #[test]
    fn sector_ord_descending() {
        // Closer to EOF (b) is "less than" earlier (a) per spec §2.6.
        assert!(sec(200, 16) > sec(100, 16));
        assert!(sec(100, 16) < sec(200, 16));
    }

    #[test]
    fn sector_eq_compares_offset_and_length() {
        assert_eq!(sec(100, 16), sec(100, 16));
        assert_ne!(sec(100, 16), sec(100, 32));
    }

    #[test]
    fn sector_is_copy() {
        let a = sec(10, 5);
        let b = a;
        assert_eq!(a, b);
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
