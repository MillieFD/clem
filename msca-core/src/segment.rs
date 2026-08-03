/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! Binary encoding and decoding for on-disk segments.
//!
//! ---
//!
//! ### Segment Composition
//!
//! A [msca](crate) file is partitioned into self-describing **segments** which are immutable once
//! written. Each segment begins with a minimal header, consisting of a [`variant: u8`](Variant)
//! identifier and [`length: NonZeroU64`](NonZeroU64), followed by a variant-specific payload.
//!
//! - [`Schema`] segments describe the structure of encoded data.
//! - [`Data`] segments carry columnar buffers for a specified schema instance.
//! - [`Binary`](crate::binary::Bin) segments carry free-form immutable bytes in any format.
//!
//! Multimodality and schema evolution are realised by appending additional schema segments. Data
//! storage and file extensibility are realised by appending additional data segments. Format
//! extensibility may be achieved via the introduction of new segment variants in future releases.

#![doc = include_str!("../../doc/simd-alignment.md")]

use std::convert::Infallible;
use std::fmt::{Display, Formatter};
use std::num::NonZeroU64;

use minicbor::{CborLen, Decode, Encode};
use smol::io::{AsyncSeek, AsyncWrite, AsyncWriteExt};

use crate::io::{self, Buffer, Checksum};
use crate::schema::number;
use crate::{Sector, Serialize};

/* ------------------------------------------------------------------------------ Public Exports */

#[rustfmt::skip] // pub exports grouped separately from private imports
pub use self::variant::Variant;

/// A minimal segment header containing:
///
/// 1. A [`Variant`] byte to identify the segment type and payload structure.
/// 2. LE [`NonZeroU64`] encoding the size of the segment payload in bytes.
///
/// See the [module level documentation](self) for more details.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
pub(crate) struct Header {
    /// Segment [`Variant`] identifier carried in the first byte of every segment header.
    #[n(0)]
    variant: Variant,
    /// LE [`NonZeroU64`] encoding the exact size of the segment body in bytes. Excludes the header
    /// and checksum.
    #[n(1)]
    size: NonZeroU64,
}

impl Header {
    /// Packed [size](size_of) of the segment [`Header`] in bytes.
    ///
    /// The rust compiler may add padding bytes to the **in-memory** header struct to improve field
    /// alignment and access efficiency. However, the **on-disk** segment header requires a strict
    /// compact layout without padding to minimise storage overhead.
    pub(crate) const SIZE: usize = size_of::<u8>() + size_of::<NonZeroU64>();
}

impl Serialize for Header {
    type Buffer = [u8; Self::SIZE];

    fn size(&self) -> Result<NonZeroU64, number::Error>
    where
        Self: Sized,
    {
        { Self::SIZE as u64 }.try_into().map_err(number::Error::Convert)
    }

    fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], number::Error> {
        // SAFETY: Header::Buffer size is Σ of fixed-size fields; guaranteed to fit all data.
        buf.serialize_push(&self.variant)?.serialize_push(&self.size)
    }

    fn serialize(&self) -> Result<Self::Buffer, number::Error> {
        [u8::MIN; Self::SIZE].serialize_push(self)
    }
}

mod variant {
    //! This module defines the segment [`Variant`] identifier and associated parsing [`Error`].
    //!
    //! A [msca](crate) file is partitioned into self-describing **segments** which are immutable
    //! once written. Each segment begins with a single [`Variant`] byte to identify the segment
    //! type and payload structure. Readers dispatch on the variant byte to specific decoders.

    use std::fmt::{Display, Formatter};
    use std::num::NonZeroU64;

    use minicbor::{CborLen, Decode, Encode};

    use crate::Serialize;
    use crate::io::Buffer;
    use crate::schema::number;

    /* -------------------------------------------------------------------------- Public Exports */

    /// On-disk **variant** identifier carried in the first byte of every segment header.
    ///
    /// Format extensibility may be achieved via the introduction of new segment variants in future
    /// releases. Existing variants are guaranteed to retain their discriminant values for binary
    /// compatibility with existing files.
    ///
    /// See the [module level documentation](self) for more details.
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
    #[non_exhaustive] // To accommodate future segment variants.
    #[repr(u8)] // To map discriminant values directly ⇄ variant byte in the segment header.
    pub enum Variant {
        /// The file [`Manifest`][1] written immediately after the immutable segment region.
        ///
        /// [1]: crate::manifest::Manifest
        #[n(0)]
        Manifest = 0x00, // DO NOT alter discriminant value (breaking change)
        /// A [`Schema`] segment describing the [structure](crate::schema::Schema) of encoded data.
        #[n(1)]
        Schema = 0x01, // DO NOT alter discriminant value (breaking change)
        /// A [`Data`] segment encoding columnar buffers for a specified schema instance.
        #[n(2)]
        Data = 0x02, // DO NOT alter discriminant value (breaking change)
        /// A free-form [`Binary`][1] segment containing immutable bytes in any user-defined format.
        ///
        /// [1]: crate::binary::Bin
        #[n(3)]
        Binary = 0x03, // DO NOT alter discriminant value (breaking change)
    }

    /* ------------------------------------------------------------------- Trait Implementations */

    impl Display for Variant {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Manifest => write!(f, "Manifest"),
                Self::Schema => write!(f, "Schema"),
                Self::Data => write!(f, "Data"),
                Self::Binary => write!(f, "Binary"),
            }
        }
    }

    impl TryFrom<u8> for Variant {
        type Error = Error;

        fn try_from(byte: u8) -> Result<Self, Error> {
            match byte {
                x if x == Self::Manifest as u8 => Ok(Self::Manifest),
                x if x == Self::Schema as u8 => Ok(Self::Schema),
                x if x == Self::Data as u8 => Ok(Self::Data),
                x if x == Self::Binary as u8 => Ok(Self::Binary),
                other => Error::Unknown { found: other }.into(),
            }
        }
    }

    impl Serialize for Variant {
        type Buffer = [u8; size_of::<u8>()];

        fn size(&self) -> Result<NonZeroU64, number::Error> {
            { *self as u8 }.size()
        }

        fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], number::Error> {
            { *self as u8 }.serialize_into(buf)
        }

        fn serialize(&self) -> Result<Self::Buffer, number::Error> {
            [u8::MIN; size_of::<u8>()].serialize_push(self)
        }
    }

    /* -------------------------------------------------------------------------- Specific Error */

    /// Errors returned by [`Variant`] parsing.
    ///
    /// Enum variants cover various granular error cases that may arise when working with segments.
    /// Users should consider handling errors explicitly wherever possible to provide meaningful
    /// error messages and recovery actions.
    ///
    /// ### Implementation
    ///
    /// This enum is `#[non_exhaustive]` meaning additional variants may be added in future versions.
    /// Implementers are advised to include a wildcard arm `_` to account for potential additions.
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
    #[non_exhaustive] // To accommodate potential future error cases.
    pub enum Error {
        /// The actual variant byte did not match the [`Variant`] expected by the caller.
        #[n(0)]
        Unexpected {
            /// The [`Variant`] byte expected by the caller.
            #[n(0)]
            expected: u8,
            /// The actual [`Variant`] byte encountered by the caller.
            #[n(1)]
            found: u8,
        },
        /// The actual variant byte did not map to any known [`Variant`].
        #[n(1)]
        Unknown {
            /// The actual [`Variant`] byte encountered by the caller.
            #[n(0)]
            found: u8,
        },
    }

    impl Display for Error {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            match self {
                Error::Unexpected { found, .. } => write!(f, "Unexpected variant → 0x{found:02X}"),
                Error::Unknown { found } => write!(f, "Unknown variant → 0x{found:02X}"),
            }
        }
    }

    impl std::error::Error for Error {}

    impl From<u8> for Error {
        fn from(value: u8) -> Self {
            Self::Unknown { found: value }
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
}

/* -------------------------------------------------------------------- Segment Trait Definition */

/// A self-describing on-disk **segment**
///
/// ### Segment Composition
///
/// Data is recorded using self-describing segments which are immutable once written. Each segment
/// begins with a minimal **segment header** consisting of a [`Variant`] identifier and `u64` size.
///
/// ```text
/// [Variant] [Size] [Body] ... [Checksum]
/// ```
///
/// Segments are **not aligned** by default. Specific [segment variants](Variant) can override the
/// default implementation to achieve [64-bit alignment](Align) by inserting up to seven bytes of
/// zero-filled padding between the segment body and [`Checksum`].
///
/// Refer to the [module level documentation](self) for more details.
pub(crate) trait Segment: Checksum + Sized {
    /// On-disk variant identifier for [`Self`]. Stored in the first byte of the segment header.
    const VARIANT: Variant;

    /// Wrap the provided `self` body with the generated segment [`Header`] and [`Checksum`].
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Zero`][1] on `u64` or `usize` overflow.
    ///
    /// [1]: number::Error::Zero
    fn wrap(&self, offset: u64) -> Result<Vec<u8>, number::Error>;

    /// Write [`Self`] to the [`file`](F). Returns the written [`Sector`] for subsequent function
    /// chaining.
    ///
    /// ### Errors
    ///
    /// - [`Error::Io`](io::Error::Io) if the underlying [`seek`](Sector::seek_to_start) or
    ///   [`write`](AsyncWriteExt::write_all) fails.
    /// - [`Error::Number`](io::Error::Number) if the [`Sector`] overflows `u64` or `usize`.
    async fn write<F>(&self, file: &mut F, offset: u64) -> Result<Sector, io::Error>
    where
        F: AsyncSeek + AsyncWrite + Unpin,
    {
        let buf = self.wrap(offset)?;
        let sector = Sector::new(offset, buf.size()?)?;
        sector.seek_to_start(file).await?;
        file.write_all(&buf).await?;
        Ok(sector)
    }
}

/* --------------------------------------------------------------------------- Alignment Helpers */

/// A **numeric type** that can be rounded up to the next 64-bit SIMD [alignment boundary](self).
#[doc(hidden)] // Reachable through SizedBuf serialization
pub trait Align {
    /// Round `self` up ↑ to the next 64-bit SIMD [alignment boundary](self).
    ///
    /// ### Errors
    ///
    /// Returns [`number::Error`] if `self` overflows `u64` or conversion into `u64` fails. Refer to
    /// each implementation for specific error cases.
    // NOTE: all on-disk sizes and offsets use u64; usize would truncate on 32-bit targets.
    fn align(self) -> Result<u64, number::Error>;

    /// Calculate the number of trailing zero bytes required to align `self` to the next 64-bit SIMD
    /// [alignment boundary](self).
    ///
    /// ### Errors
    ///
    /// Returns [`number::Error`] if `self` overflows `u64` or conversion into `u64` fails. Refer to
    /// each implementation for specific error cases.
    fn pad(self) -> Result<usize, number::Error>;
}

/// Blanket implementation covering all types that fallibly convert to [`u64`], enabling chained
/// alignment arithmetic such as `sector.next().ok_or(Error::Zero)?.align()?`.
impl<I> Align for I
where
    I: TryInto<u64>,
    number::Error: From<I::Error>,
{
    fn align(self) -> Result<u64, number::Error> {
        let n = self.try_into()?.next_multiple_of(8);
        Ok(n)
    }

    fn pad(self) -> Result<usize, number::Error> {
        let n = self.try_into()?;
        // NOTE: pad ≤ seven bytes; conversion to usize is infallible in practice.
        n.next_multiple_of(8).saturating_sub(n).try_into().map_err(number::Error::Convert)
    }
}

/* ------------------------------------------------------------------------------ Specific Error */

/// Errors returned by [`Segment`] operations such as [serialisation](Serialize) and
/// [deserialisation](Deserialize).
///
/// Enum variants cover various granular error cases that may arise when working with segments.
/// Users should consider handling errors explicitly wherever possible to provide meaningful error
/// messages and recovery actions.
///
/// ### Implementation
///
/// This enum is `#[non_exhaustive]` meaning additional variants may be added in future versions.
/// Implementers are advised to include a wildcard arm `_` to account for potential additions.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Encode, Decode, CborLen)]
#[non_exhaustive] // To accommodate potential future error cases.
pub enum Error {
    /// Underlying [`variant::Error`] from a failed [`Variant`] parsing operation.
    #[n(0)]
    Variant(#[n(0)] variant::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Variant(error) => write!(f, "Segment variant ID error → {error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<variant::Error> for Error {
    fn from(error: variant::Error) -> Self {
        Self::Variant(error)
    }
}

impl From<Infallible> for Error {
    fn from(value: Infallible) -> Self {
        match value {}
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
    use super::*;
    use crate::io::{self, Checksum};
    use crate::schema::Schema;

    /* ---------------------------------------------------------------------------- Shared State */

    /// A single-column `u32` [`Schema`] named `t`, shared by the framing tests.
    fn schema() -> Schema {
        let mut schema = Schema::new("t");
        schema.column::<u32>("v").expect("Column failed");
        schema
    }

    /* ------------------------------------------------------------------------------ Unit Tests */

    /// [`Align::align`] rounds up ↑ to the next multiple of **eight**; idempotent at boundaries.
    #[test]
    fn align_rounds_up_to_eight() {
        assert_eq!(0u64.align().expect("Align 0 → 0 failed"), 0);
        assert_eq!(1u64.align().expect("Align 1 → 8 failed"), 8);
        assert_eq!(7u64.align().expect("Align 7 → 8 failed"), 8);
        assert_eq!(8u64.align().expect("Align 8 → 8 failed"), 8);
        assert_eq!(9u64.align().expect("Align 9 → 16 failed"), 16);
        assert_eq!(64u64.align().expect("Align 64 → 64 failed"), 64);
    }

    /// [`Align::pad`] counts the number of bytes required to reach the next alignment boundary.
    #[test]
    fn pad_counts_bytes_to_boundary() {
        assert_eq!(0u64.pad().expect("Pad 0 → 0 failed"), 0);
        assert_eq!(1u64.pad().expect("Pad 1 → 7 failed"), 7);
        assert_eq!(8u64.pad().expect("Pad 8 → 0 failed"), 0);
        assert_eq!(14u64.pad().expect("Pad 14 → 2 failed"), 2);
    }

    /// [`Align`] is implemented for every type that converts into [`u64`].
    #[test]
    fn align_accepts_any_into_u64() {
        assert_eq!(7u8.align().expect("Align u8 failed"), 8);
        assert_eq!(9u32.align().expect("Align u32 failed"), 16);
        assert_eq!(33usize.align().expect("Align usize failed"), 40);
        assert_eq!(NonZeroU64::MIN.align().expect("Align NonZeroU64 failed"), 8);
    }

    /// [`Segment::wrap`] lays out `[variant][size][payload][checksum]` densely for an off-hot-path
    /// [`Schema`] segment: the `size` field spans exactly the payload with no trailing padding, the
    /// framed footprint is the header plus payload plus checksum, and the trailing
    /// [checksum](Checksum::verify) covers every preceding byte.
    #[test]
    fn schema_frame_is_dense() {
        let schema = schema();
        let bytes = schema.wrap(0).expect("Wrap failed");
        let head = Header::SIZE;
        let size = u64::from_le_bytes(bytes[1..head].try_into().expect("Size is 8 bytes")) as usize;
        let body = Schema::verify(&bytes).expect("Checksum failed");
        assert_eq!(bytes[0], Variant::Schema as u8);
        assert_eq!(size, schema.size().expect("Size failed").get() as usize); // densely packed
        assert_eq!(bytes.len(), head + size + size_of::<u64>());
        assert_eq!(body, &bytes[..bytes.len() - size_of::<u64>()]); // checksum suffix verifies
    }

    /// A single-byte mutation anywhere in the framed payload changes the computed checksum, so
    /// [`Checksum::verify`] rejects the corrupt frame while accepting the intact one.
    #[test]
    fn frame_checksum_detects_mutation() {
        let bytes = schema().wrap(0).expect("Wrap failed");
        let mut corrupt = bytes.clone();
        let at = corrupt.len() - size_of::<u64>();
        corrupt[at - 1] ^= u8::MAX; // flip a payload byte
        let intact = Schema::verify(&bytes).expect("Intact frame rejected");
        let error = Schema::verify(&corrupt).expect_err("Corruption undetected");
        assert_eq!(intact, &bytes[..bytes.len() - size_of::<u64>()]);
        assert!(matches!(error, io::Error::Checksum));
    }

    /// [`Variant::try_from`] maps the manifest discriminant `0x00`.
    #[test]
    fn variant_maps_manifest_discriminant() {
        assert_eq!(Variant::try_from(0x00), Ok(Variant::Manifest));
    }

    /// [`Variant::try_from`] maps the binary discriminant `0x03`.
    #[test]
    fn variant_maps_binary_discriminant() {
        assert_eq!(Variant::try_from(0x03), Ok(Variant::Binary));
    }
}
