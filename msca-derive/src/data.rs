/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! Procedural macro expansion logic for `#[derive(Data)]`.
//!
//! ### Using `#[derive(Data)]`
//!
//! Add the attribute to any algebraic data type.
//!
//! ```rust,ignore
//! #[derive(Data)]
//! struct Record {
//!     uuid: u8,
//!     latitude: f64,
//!     longitude: f64,
//! }
//! ```
// TODO → Document new schema and empty accumulator initialisation from Dataset once API exists
//!
//! Field registration is determined by the field [`Type`](syn::Type):
//!
//! - Supported primitive types register a corresponding column in the `Schema`.
//! - Algebraic types defer to their own `#[derive(Data)]` implementation
//!
//! This design allows for recursive nesting of `#[derive(Data)]` types which flatten into a single
//! collection of primitive columns. Fields are processed in **name-sorted** order corresponding to
//! the deterministic platform-invariant [`BTreeMap`][1] column order used throughout [msca](crate).
//!
//! ### Expansion
//!
//! Generated code lives inside an anonymous `const` block to avoid collision with user items.
//!
//! 1. A hidden composite accumulator type holding one concrete sub-accumulator per field.
//! 2. An `Accumulate` implementation distributing pushed items across the sub-accumulators.
//! 3. A `Describe` implementation threading buffer descriptors through each sub-accumulator.
//! 4. A `Serialize` implementation chaining sub-accumulators into one contiguous buffer.
//! 5. A `Data` implementation to register columns and construct the composite accumulator.
//!
//! [1]: std::collections::BTreeMap

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{DeriveInput, Ident, Visibility};

use crate::{Field, fields};

/* ------------------------------------------------------------------------------ Public Exports */

/// Expand `#[derive(Data)]` according to the [module-level documentation](self).
///
/// ### Errors
///
/// Returns [`syn::Error`] if the input is not supported, has unnamed fields, or has no fields.
pub(crate) fn expand(input: &DeriveInput) -> Result<TokenStream, syn::Error> {
    // 1. Resolve struct names
    let src = &input.ident;
    let acc = &format_ident!("{src}Accumulator");
    let vis = &input.vis;
    // 2. Extract and sort fields by name
    let fields = &fields(input)?;
    // 3. Generate trait implementations
    let accumulator = accumulator(vis, acc, fields);
    let accumulate = accumulate(src, acc, fields);
    let describe = describe(src, acc, fields);
    let serialize = serialize(acc, fields);
    let data = data(src, acc, fields);
    // 4. Wrap in an anonymous const block
    Ok(quote! {
        const _: () = {
            #accumulator
            #accumulate
            #describe
            #serialize
            #data
        };
    })
}

/* ----------------------------------------------------------------------- TokenStream Expansion */

/// Generate the composite **accumulator** type holding one concrete sub-accumulator per field,
/// named rather than boxed so the whole tree monomorphizes and inlines.
///
/// The accumulator appears in the public [`Data::Acc`] associated type, so it inherits the source
/// visibility to avoid leaking a private type through the public interface.
fn accumulator(vis: &Visibility, acc: &Ident, fields: &[Field]) -> TokenStream {
    let idents = Field::idents(fields);
    let types = Field::types(fields);
    quote! {
        /// Generated composite accumulator holding one concrete sub-accumulator per field.
        #[derive(::core::default::Default)]
        #vis struct #acc {
            #( #idents: ::msca::accumulate::Buffer<#types>, )*
        }
    }
}

/// Implement `Accumulate` for the generated [`accumulator`].
///
/// - `push` distributes each incoming field across the sub-accumulators.
/// - `discard` delegates to each sub-accumulator in order.
/// - `is_empty` and `count` delegate to the first field only.
///
/// A composite spans one buffer **per field** rather than one buffer overall, so it implements no
/// `Descriptor`: each field column registers its own descriptor through `Describe::buffers`.
///
/// This design ensures that all sub-accumulators advance in lockstep.
fn accumulate(src: &Ident, acc: &Ident, fields: &[Field]) -> TokenStream {
    let idents = Field::idents(fields);
    // NOTE: crate::fields rejects empty structs; the first field is guaranteed to exist
    let head = idents[0];
    quote! {
        impl ::msca::Accumulate<#src> for #acc {
            fn push(&mut self, value: #src) {
                #( self.#idents.push(value.#idents); )*
            }

            fn discard(&mut self) {
                #( self.#idents.discard(); )*
            }

            fn is_empty(&self) -> bool {
                self.#head.is_empty()
            }

            fn count(&self) -> u64 {
                self.#head.count()
            }

        }
    }
}

/// Implement `Describe` for the generated [`accumulator`].
///
/// - `buffers` threads the `offset` through each sub-accumulator in order to encode buffers
///   contiguously, returning the final offset.
fn describe(src: &Ident, acc: &Ident, fields: &[Field]) -> TokenStream {
    let idents = Field::idents(fields);
    quote! {
        impl ::msca::Describe<#src> for #acc {
            fn buffers(
                &self,
                offset: u64,
                columns: &mut ::msca::Columns,
            ) -> ::core::result::Result<u64, ::msca::schema::Error> {
                #( let offset = self.#idents.buffers(offset, columns)?; )*
                ::core::result::Result::Ok(offset)
            }
        }
    }
}

/// Implement `Serialize` for the generated [`accumulator`].
///
/// - `size` folds the framed `SizedBuf` footprint (length prefix + aligned payload) of every
///   sub-accumulator.
/// - `serialize_into` frames each sub-accumulator as one `SizedBuf` region in order.
/// - `serialize` allocates using `size` and fills via `serialize_into`.
fn serialize(acc: &Ident, fields: &[Field]) -> TokenStream {
    let idents = Field::idents(fields);
    quote! {
        impl ::msca::Serialize for #acc {
            type Buffer = ::std::vec::Vec<u8>;

            fn size(
                &self,
            ) -> ::core::result::Result<::core::num::NonZeroU64, ::msca::schema::number::Error> {
                let total = [ #( ::msca::SizedBuf::new(&self.#idents).size(), )* ]
                    .into_iter()
                    .try_fold(u64::MIN, |acc, size| {
                        let size = size?.get();
                        acc.checked_add(size).ok_or(::msca::schema::number::Error::Zero)
                    })?;
                ::core::num::NonZeroU64::new(total).ok_or(::msca::schema::number::Error::Zero)
            }

            fn serialize_into<'a>(
                &self,
                buf: &'a mut [u8],
            ) -> ::core::result::Result<&'a mut [u8], ::msca::schema::number::Error> {
                #( let buf = ::msca::SizedBuf::new(&self.#idents).serialize_into(buf)?; )*
                ::core::result::Result::Ok(buf)
            }

            fn serialize(
                &self,
            ) -> ::core::result::Result<::std::vec::Vec<u8>, ::msca::schema::number::Error> {
                let size = self
                    .size()?
                    .get()
                    .try_into()
                    .map_err(::msca::schema::number::Error::from)?;
                let mut buf = ::std::vec![0u8; size];
                self.serialize_into(&mut buf)?;
                // NOTE: cannot use static assertion; size depends on runtime data accumulation
                ::core::debug_assert_eq!(buf.len(), size, "actual size ≠ predicted size");
                ::core::result::Result::Ok(buf)
            }
        }
    }
}

/// Implement `Data` for the annotated external type.
fn data(src: &Ident, acc: &Ident, fields: &[Field]) -> TokenStream {
    let idents = Field::idents(fields);
    let types = Field::types(fields);
    let names = Field::names(fields);
    quote! {
        impl ::msca::Data for #src {
            type Acc = #acc;

            fn accumulator(
                schema: &mut ::msca::Schema,
            ) -> ::core::result::Result<Self::Acc, ::msca::schema::Error> {
                ::core::result::Result::Ok(#acc {
                    #( #idents: schema.column::<#types>(#names)?, )*
                })
            }
        }
    }
}

/* --------------------------------------------------------------------------------------- Tests */

#[cfg(test)]
mod tests {
    use syn::parse_quote;

    use super::*;
    use crate::tests::{has, row};

    /* ---------------------------------------------------------------------------- Shared State */

    /// Expand the shared [`row`] and render the generated tokens as one string to search.
    fn code() -> String {
        expand(&row()).expect("Expansion failed").to_string()
    }

    /* ------------------------------------------------------------------------------ Unit Tests */

    /// [`expand`] emits the hidden accumulator and one implementation per generated trait.
    #[test]
    fn expand_emits_accumulator_and_impls() {
        let code = code();
        assert!(has(&code, "struct RowAccumulator"));
        assert!(has(
            &code,
            "impl ::msca::Accumulate<Row> for RowAccumulator"
        ));
        assert!(has(&code, "impl ::msca::Serialize for RowAccumulator"));
        assert!(has(&code, "impl ::msca::Data for Row"));
        assert!(has(&code, "type Acc = RowAccumulator"));
        assert!(!has(&code, "BoxAcc")); // the accumulator tree is no longer type-erased
    }

    /// [`expand`] frames each sub-accumulator as one `SizedBuf` region.
    #[test]
    fn expand_frames_each_column() {
        assert!(has(&code(), "SizedBuf"));
    }

    /// [`expand`] output parses as valid Rust.
    #[test]
    fn expand_output_parses() {
        let expanded = expand(&row()).expect("Expansion failed");
        syn::parse2::<syn::File>(expanded).expect("Generated code does not parse");
    }

    /// [`expand`] emits **no** `Descriptor` for the composite accumulator.
    ///
    /// A composite spans one buffer per field rather than one buffer overall, so it has no single
    /// descriptor to give; each field column registers its own through `Describe::buffers`.
    #[test]
    fn expand_composite_has_no_descriptor() {
        let code = code();
        assert!(!has(&code, "Descriptor"));
        assert!(!has(&code, "fn describe"));
    }

    /// [`expand`] rejects inputs without named fields.
    ///
    /// Field names are required to generate column names in the `Schema`.
    #[test]
    fn expand_rejects_tuple_struct() {
        let input: DeriveInput = parse_quote! { struct Tuple(u32); };
        expand(&input).expect_err("Unsupported input accepted");
    }
}
