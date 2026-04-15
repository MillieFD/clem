/*
Project: clem
GitHub: https://github.com/MillieFD/clem

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

use std::fmt;

/* ----------------------------------------------------------------------------- Public Exports */

/// Errors returned by [`clem`](crate).
///
/// Enum variants cover various granular error cases that may arise when working with datasets,
/// schemas, or column operations. Users should consider handling errors explicitly wherever
/// possible to provide meaningful error messages and recovery actions.
///
/// ### Implementation
///
/// This enum is `#[non_exhaustive]` meaning additional variants may be added in future versions.
/// Implementers are advised to include a wildcard arm `_` to account for potential additions.
#[non_exhaustive]
#[derive(Debug)]
pub enum Error {
    /// Underlying [`std::io::Error`] from the file backing the [`Dataset`].
    Io(std::io::Error),
    /// Underlying [`std::str::Utf8Error`] while attempting to interpret `[u8]` as a [`String`].
    Utf8(std::str::Utf8Error),
    /// Underlying [`std::num::TryFromIntError`] from a checked conversion between two types.
    Convert(std::num::TryFromIntError),
    /// CBOR encoding failure for a manifest or schema payload.
    Encode(String),
    /// CBOR decoding failure for a manifest or schema payload.
    Decode(minicbor::decode::Error),
    /// File magic bytes did not match the expected `clem` signature.
    Magic,
    /// File version not recognised by this build of [`clem`](crate).
    Version(u8),
}

/* ----------------------------------------------------------------------- Trait Implementations */

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "File IO error → {e}"),
            Self::Utf8(e) => write!(f, "UTF8 from u8 error → {e}"),
            Self::Convert(e) => write!(f, "Integer type conversion error → {e}"),
            Self::Encode(msg) => write!(f, "CBOR encode error → {msg}"),
            Self::Decode(e) => write!(f, "CBOR decode error → {e}"),
            Self::Magic => f.write_str("File is not a valid `clem` dataset"),
            Self::Version(v) => write!(f, "Unrecognised version → {v}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Utf8(e) => Some(e),
            Self::Convert(e) => Some(e),
            Self::Decode(e) => Some(e),
            _ => None, // Some variants do not wrap an inner error source
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(error: std::str::Utf8Error) -> Self {
        Self::Utf8(error)
    }
}

impl From<std::num::TryFromIntError> for Error {
    fn from(error: std::num::TryFromIntError) -> Self {
        Self::Convert(error)
    }
}

impl<E> From<minicbor::encode::Error<E>> for Error
where
    Error: for<'a> From<&'a E>,
    E: Into<Error> + fmt::Display,
{
    fn from(error: minicbor::encode::Error<E>) -> Self {
        match error.as_write() {
            Some(e) => e.into(),
            None => Self::Encode(error.to_string()),
        }
    }
}

impl From<minicbor::decode::Error> for Error {
    fn from(error: minicbor::decode::Error) -> Self {
        Self::Decode(error)
    }
}

impl From<std::convert::Infallible> for Error {
    fn from(_: std::convert::Infallible) -> Self {
        unreachable!()
    }
}

/* --------------------------------------------------------------------------------------- Tests */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_io_error() {
        use std::io::{self, ErrorKind};
        let source = io::Error::new(ErrorKind::Other, "Test IO error");
        let error: Error = source.into();
        assert_eq!(error.to_string(), "File IO error → Test IO error");
    }

    #[test]
    fn from_utf8_error() {
        use std::str;
        let source = str::from_utf8(b"\xFF").unwrap_err();
        let error: Error = source.into();
        assert!(error.to_string().starts_with("UTF8 from u8 error →"));
    }
}
