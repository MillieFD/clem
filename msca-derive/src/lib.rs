/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! Procedural macros for the `msca` storage engine.
//!
//! ---
//!
//! Each macro expansion is implemented in the corresponding submodule; refer to the module-level
//! documentation for more details. Generated code resolves all paths via the `msca` facade which
//! re-exports this crate. Standalone use of `msca-derive` is not supported.

#![doc = include_str!("../../doc/derive.md")]

mod data;
mod read;

use proc_macro::TokenStream;
use syn::{parse_macro_input, Data, DataStruct, DeriveInput, Fields, Ident, Type};

/* ------------------------------------------------------------------------------ Public Exports */

/// Implement the `Data` trait and supporting machinery.
///
/// Refer to the [module-level documentation](data) for more details.
#[proc_macro_derive(Data)]
pub fn data(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    data::expand(&input).unwrap_or_else(syn::Error::into_compile_error).into()
}

/// Implement the `Read` trait and supporting machinery.
///
/// Refer to the [module-level documentation](read) for more details.
#[proc_macro_derive(Read)]
pub fn read(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    read::expand(&input).unwrap_or_else(syn::Error::into_compile_error).into()
}

/* ---------------------------------------------------------------------------- Field Extraction */

/// A single field from the external struct; borrows from [`DeriveInput`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Field<'a> {
    /// Field [identifier](Ident); used to generate the corresponding schema column name.
    ident: &'a Ident,
    /// Field [type](Type); parameterises the generated column accumulator or stream.
    ty: &'a Type,
}

impl<'a> Field<'a> {
    /// Returns the [identifier](Ident) for each [`Field`].
    fn idents(fields: &[Self]) -> Vec<&'a Ident> {
        fields.iter().map(|field| field.ident).collect()
    }

    /// Returns the [`Type`] for each [`Field`].
    fn types(fields: &[Self]) -> Vec<&'a Type> {
        fields.iter().map(|field| field.ty).collect()
    }

    /// Returns the column [`name`](String) for each [`Field`].
    fn names(fields: &[Self]) -> Vec<String> {
        let name = |field: &Self| field.ident.to_string();
        fields.iter().map(name).collect()
    }
}

/// Extract [fields](Field) from an external type in **name-sorted** order to match the
/// platform-invariant deterministic [`BTreeMap`][1] column order used throughout [msca](crate).
///
/// ### Errors
///
/// Returns [`syn::Error`] if the input is not supported, has unnamed fields, or has no fields.
///
/// [1]: std::collections::BTreeMap
fn fields<'a>(input: &'a DeriveInput) -> Result<Vec<Field<'a>>, syn::Error> {
    let error = |msg| Err(syn::Error::new_spanned(input, msg));
    let named = match &input.data {
        Data::Struct(DataStruct { fields: Fields::Named(named), .. }) => &named.named,
        Data::Struct(..) => return error("msca requires named fields to generate a schema"),
        other => return error("msca does not currently support this type"),
    };
    let mut fields: Vec<Field> = named
        .iter()
        .filter_map(|field| field.ident.as_ref().map(|ident| Field { ident, ty: &field.ty }))
        .collect();
    match fields.is_empty() {
        true => return error("this type has no fields"),
        false => fields.sort_by_key(|field| field.ident.to_string()),
    }
    Ok(fields)
}

/* --------------------------------------------------------------------------------------- Tests */

#[cfg(test)]
mod tests {
    use syn::parse_quote;

    use super::*;

    /* ---------------------------------------------------------------------------- Shared State */

    /// Space-insensitive containment check; tolerant of generated token spacing.
    pub(crate) fn has(code: &str, needle: &str) -> bool {
        code.replace(' ', "").contains(&needle.replace(' ', ""))
    }

    /// The two-field `struct Row` shared by the [`expand`](crate::data::expand) tests across the
    /// crate; its fields are deliberately out of order so field sorting is exercised downstream.
    pub(crate) fn row() -> DeriveInput {
        parse_quote! { struct Row { a: u32, b: f64 } }
    }

    /* ------------------------------------------------------------------------------ Unit Tests */

    /// [`fields`] returns the named fields sorted by identifier.
    #[test]
    fn fields_sort_by_identifier() {
        let input: DeriveInput = parse_quote! { struct Row { b: u8, a: u16, c: u32 } };
        let fields = fields(&input).expect("Named struct was rejected");
        assert_eq!(Field::names(&fields), ["a", "b", "c"]);
    }

    /// [`fields`] rejects enums, tuple structs, unit structs, and empty structs.
    #[test]
    fn fields_reject_unsupported_shapes() {
        let inputs: [DeriveInput; 4] = [
            parse_quote! { enum Level { Low } },
            parse_quote! { struct Tuple(u8); },
            parse_quote! { struct Unit; },
            parse_quote! { struct Empty {} },
        ];
        inputs.iter().for_each(|input| {
            fields(input).expect_err("Unsupported shape accepted");
        });
    }
}
