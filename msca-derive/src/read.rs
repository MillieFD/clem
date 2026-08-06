/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! Procedural macro expansion logic for `#[derive(Read)]`.
//!
//! ### Using `#[derive(Read)]`
//!
//! Add the attribute to any algebraic data type.
//!
//! ```rust,ignore
//! #[derive(Read)]
//! struct Record {
//!     uuid: u8,
//!     latitude: f64,
//!     longitude: f64,
//! }
//! ```
//!
//! TODO → Document query construction and composite row streaming via Dataset::query
//!
//! Field streaming is determined by the field [`Type`](syn::Type):
//!
//! - Supported primitive types stream from the corresponding column in the `Query`.
//! - Algebraic types defer to their own `#[derive(Read)]` implementation
//!
//! This design allows for recursive nesting of `#[derive(Read)]` types which rebuild from a flat
//! collection of primitive columns. Fields are processed in **name-sorted** order corresponding to
//! the deterministic platform-invariant [`BTreeMap`][1] column order used throughout [msca](crate).
//!
//! ### Expansion
//!
//! Generated code lives inside an anonymous `const` block to avoid collision with user items.
//!
//! 1. A composite reader type holding one boxed sub-stream per field.
//! 2. One `Composite` implementation, generic over any combination of the named columns.
//! 3. An `Iterator` implementation pulling one item per sub-stream in lockstep.
//! 4. An `Unfiltered` implementation combining every column with nothing attached.
//!
//! A filtered read and an unfiltered one therefore reach their streams by the **same** route: the
//! unfiltered path builds the default conjunction and hands it to the same `Composite`.
//!
//! [1]: std::collections::BTreeMap

use proc_macro2::TokenStream;
use quote::{ToTokens, format_ident, quote};
use syn::{DeriveInput, Ident, Visibility};

use crate::{Field, fields};

/* ------------------------------------------------------------------------------ Public Exports */

/// Expand `#[derive(Read)]` according to the [module-level documentation](self).
///
/// ### Errors
///
/// Returns [`syn::Error`] if the input is not supported, has unnamed fields, or has no fields.
pub(crate) fn expand(input: &DeriveInput) -> Result<TokenStream, syn::Error> {
    // 1. Resolve struct names and visibility
    let src = &input.ident;
    let reader = &format_ident!("{src}Reader");
    let vis = &input.vis;
    // 2. Extract and sort fields by name
    let fields = &fields(input)?;
    // 3. Generate the reader struct and its trait implementations
    let structure = structure(vis, reader, fields);
    let composite = composite(src, reader, fields);
    let iterate = iterate(src, reader, fields);
    let unfiltered = unfiltered(src, reader, fields);
    // 4. Wrap in an anonymous const block to avoid collision with user items
    Ok(quote! {
        const _: () = {
            #structure
            #composite
            #iterate
            #unfiltered
        };
    })
}

/* ----------------------------------------------------------------------- TokenStream Expansion */

/// Generate the composite **reader struct**: one boxed column stream per field.
///
/// Each field holds a type-erased [`Outcome`] iterator, because the opaque stream types of the
/// columns differ and a struct field must name one concrete type. The reader appears in the public
/// `Read::Src` GAT, so it inherits the source visibility to avoid leaking a private type through
/// the public interface.
fn structure(vis: &Visibility, reader: &Ident, fields: &[Field]) -> TokenStream {
    let idents = Field::idents(fields);
    let types = Field::types(fields);
    quote! {
        /// Generated composite reader: one boxed column stream per field.
        ///
        /// The stream types differ per column and are opaque, so each field is type-erased. `S`
        /// carries the source shape, which is what lets the fold monomorphize.
        #vis struct #reader<'a, S> {
            #(
                #idents: ::std::boxed::Box<
                    dyn ::core::iter::Iterator<Item = ::msca::Outcome<#types>> + 'a
                >,
            )*
            src: ::core::marker::PhantomData<S>,
        }
    }
}

/// Implement `Composite` **once**, generic over any combination of the named columns.
///
/// The source arrives unresolved. A selection is minted here and narrowed through every filter of
/// every leg before a single stream is built, so a buffer one leg cleared costs its siblings no
/// file IO. The resolved tree is then walked leg by leg through `Combine::unpack`, because a
/// composite reaches its legs through a type parameter where field access is unavailable.
///
/// `S` is the source combination, `N` a node of the resolved tree, and `B` a resolved leg.
fn composite(src: &Ident, reader: &Ident, fields: &[Field]) -> TokenStream {
    let idents = Field::idents(fields);
    let nodes = nodes(fields.len());
    let builds = builds(fields.len());
    let bounds = descent(&nodes, &builds, fields);
    let unpack = unpack(&idents, fields.len());
    let build = build(&idents);
    quote! {
        impl<'a, S, #(#nodes,)* #(#builds,)*> ::msca::Composite<'a, S> for #src
        where
            S: ::msca::query::Select<'a>,
            #bounds
        {
            type Reader = #reader<'a, <S as ::msca::query::Select<'a>>::Build>;

            fn new(src: S) -> ::core::result::Result<Self::Reader, ::msca::query::Error> {
                let query = ::msca::query::Select::query(&src);
                let mut mask = query.mask();
                let node = ::msca::query::Select::select(src, &mut mask)?;
                #unpack
                #build
                let src = ::core::marker::PhantomData;
                ::core::result::Result::Ok(#reader { #( #idents, )* src })
            }
        }
    }
}

/// The node type parameters `N1, N2, …` naming each **interior** level of the resolved tree.
///
/// The root is the resolved source itself, so a tree over `count` legs contributes `count - 2`.
fn nodes(count: usize) -> Vec<Ident> {
    (1..count.saturating_sub(1)).map(|level| format_ident!("N{level}")).collect()
}

/// The leg type parameters `B0, B1, …`, one per resolved column, empty for a single field whose
/// leg **is** the root.
fn builds(count: usize) -> Vec<Ident> {
    match count == 1 {
        true => Vec::new(),
        false => (0..count).map(|index| format_ident!("B{index}")).collect(),
    }
}

/// The `where` bounds descending the resolved tree: one [`Combine`] step per interior level, then
/// one [`Build`] bound per leg fixing its item to the field type.
///
/// Every parameter is reached by a projection from `S`, which is what constrains them.
fn descent(nodes: &[Ident], builds: &[Ident], fields: &[Field]) -> TokenStream {
    let types = Field::types(fields);
    let mut owner = quote! { <S as ::msca::query::Select<'a>>::Build };
    let mut levels = Vec::with_capacity(fields.len());
    for depth in (1..fields.len()).rev() {
        let leg = &builds[depth];
        let next = step(nodes, builds, depth);
        levels.push(quote! { #owner: ::msca::Combine<L0 = #next, L1 = #leg>, });
        owner = next;
    }
    let head = types[0];
    match builds.is_empty() {
        true => quote! { #owner: ::msca::query::Build<'a, Item = #head>, },
        false => quote! { #(#levels)* #( #builds: ::msca::query::Build<'a, Item = #types>, )* },
    }
}

/// The type owning everything below `depth`: the interior node one level down, or the first leg
/// once the descent reaches the bottom of the tree.
fn step(nodes: &[Ident], builds: &[Ident], depth: usize) -> TokenStream {
    match depth == 1 {
        true => builds[0].to_token_stream(),
        false => nodes[depth - 2].to_token_stream(),
    }
}

/// Walk the resolved tree from the root, binding each leg to its field identifier.
///
/// The node binding is shadowed at every level, so the descent needs no numbered temporaries; the
/// bottom level binds both remaining legs at once.
fn unpack(idents: &[&Ident], count: usize) -> TokenStream {
    let head = idents[0];
    let mut steps = Vec::with_capacity(count);
    for depth in (1..count).rev() {
        let leg = idents[depth];
        let step = match depth == 1 {
            true => quote! { let (#head, #leg) = ::msca::Combine::unpack(node); },
            false => quote! { let (node, #leg) = ::msca::Combine::unpack(node); },
        };
        steps.push(step);
    }
    match count == 1 {
        true => quote! { let #head = node; },
        false => quote! { #(#steps)* },
    }
}

/// Build one boxed stream per leg from the settled selection, which the last leg takes by move.
fn build(idents: &[&Ident]) -> TokenStream {
    let last = idents.len() - 1;
    let steps = idents.iter().enumerate().map(|e| stream(e.1, e.0 == last));
    quote! { #(#steps)* }
}

/// Build one leg into its boxed stream, cloning the selection unless this leg is the `last` one.
fn stream(ident: &Ident, last: bool) -> TokenStream {
    let mask = match last {
        true => quote! { mask },
        false => quote! { ::core::clone::Clone::clone(&mask) },
    };
    quote! { let #ident = ::std::boxed::Box::new(::msca::query::Build::build(#ident, #mask)?); }
}

/// Implement `Unfiltered`: open every named column with nothing attached, combine them, and hand
/// the default conjunction to the same `Composite` a filtered read goes through.
fn unfiltered(src: &Ident, reader: &Ident, fields: &[Field]) -> TokenStream {
    let idents = Field::idents(fields);
    let names = Field::names(fields);
    let types = Field::types(fields);
    let tree = tree(fields);
    let bounds = stream_bounds(fields);
    let head = idents[0];
    let combined = idents[1..].iter().fold(
        quote! { #head },
        |acc, leg| quote! { ::msca::Join::and(#acc, #leg)? },
    );
    quote! {
        impl<'a> ::msca::Unfiltered<'a> for #src
        where
            #bounds
        {
            fn unfiltered(
                query: &'a ::msca::Query<'a>,
            ) -> ::core::result::Result<Self::Src<'a>, ::msca::query::Error> {
                #( let #idents = query.column::<#types>(#names)?; )*
                let combined = #combined;
                <#src as ::msca::Composite<'a, #tree>>::new(combined)
            }
        }

        impl ::msca::Read for #src {
            type Src<'a> = #reader<'a, #tree>;
        }
    }
}

/// The left-nested [`Conjunct`] tree over one column handle per field.
fn tree(fields: &[Field]) -> TokenStream {
    let types = Field::types(fields);
    let mut legs =
        types.iter().map(|ty| quote! { ::msca::query::Src<'a, #ty> }).collect::<Vec<TokenStream>>();
    let head = legs.remove(0);
    legs.into_iter().fold(head, |acc, leg| quote! { ::msca::Conjunct<#acc, #leg> })
}

/// The `where` bounds `Query::read` requires of each field: the field must be [`Read`] and
/// [`Clone`], its column reader must [`Deserialize`] and [`Reader`], and the schema must unfold it.
fn stream_bounds(fields: &[Field]) -> TokenStream {
    let types = Field::types(fields);
    quote! {
        #(
            #types: ::msca::Read + ::core::clone::Clone + 'a,
            <#types as ::msca::Read>::Src<'a>:
                ::msca::Deserialize<'a, Ok = <#types as ::msca::Read>::Src<'a>>
                    + ::msca::Reader<'a, #types>,
            ::msca::Schema: ::msca::schema::Unfolder<#types>,
        )*
    }
}

/// Implement [`Iterator`] for the reader: reconstruct one item per lockstep pull.
///
/// Every field stream is pulled **before** any outcome is inspected, so an item-free outcome from
/// one column cannot leave its siblings a slot behind. [`None`] from any stream ends the composite
/// stream; an error or an absent slot carries no item to rebuild from and propagates instead.
///
/// The per-slot verdicts fold through the tree the caller wrote: each node contributes its own
/// [`Combine`] operator, so the whole fold monomorphizes to one native instruction per node.
fn iterate(src: &Ident, reader: &Ident, fields: &[Field]) -> TokenStream {
    let idents = Field::idents(fields);
    let nodes = nodes(fields.len());
    let bounds = ascent(&nodes, fields.len());
    let keeps = Field::keeps(fields);
    let verdict = verdict(&nodes, &keeps);
    quote! {
        impl<'a, N, #(#nodes,)*> ::core::iter::Iterator for #reader<'a, N>
        where
            #bounds
        {
            type Item = ::msca::Outcome<#src>;

            fn next(&mut self) -> ::core::option::Option<::msca::Outcome<#src>> {
                #( let #idents = ::core::iter::Iterator::next(&mut self.#idents)?; )*
                #(
                    let (#keeps, #idents) = match #idents {
                        ::msca::Outcome::Include(item) => (true, item),
                        ::msca::Outcome::Exclude(item) => (false, item),
                        ::msca::Outcome::Error(e) => {
                            return ::core::convert::Into::into(::msca::Outcome::Error(e));
                        }
                        ::msca::Outcome::Absent => {
                            return ::core::convert::Into::into(::msca::Outcome::Absent);
                        }
                    };
                )*
                let item = #src { #( #idents, )* };
                let outcome = match #verdict {
                    true => ::msca::Outcome::Include(item),
                    false => ::msca::Outcome::Exclude(item),
                };
                ::core::convert::Into::into(outcome)
            }
        }
    }
}

/// The `where` bounds ascending the reader tree parameter: one [`Combine`] step per interior level.
///
/// Only the nodes appear, because the fold reads no leg type; a single field folds nothing at all.
fn ascent(nodes: &[Ident], count: usize) -> TokenStream {
    let mut owner = quote! { N };
    let mut levels = Vec::with_capacity(count);
    for depth in (1..count).rev() {
        let next = match depth == 1 {
            true => TokenStream::new(),
            false => nodes[depth - 2].to_token_stream(),
        };
        let level = match depth == 1 {
            true => quote! { #owner: ::msca::Combine, },
            false => quote! { #owner: ::msca::Combine<L0 = #next>, },
        };
        levels.push(level);
        owner = next;
    }
    quote! { #(#levels)* }
}

/// Fold every per-slot verdict into one, innermost pair first, applying each node operator in turn.
fn verdict(nodes: &[Ident], keeps: &[Ident]) -> TokenStream {
    let head = &keeps[0];
    let mut folded = quote! { #head };
    for depth in 1..keeps.len() {
        let keep = &keeps[depth];
        let node = match depth == keeps.len() - 1 {
            true => quote! { N },
            false => nodes[depth - 1].to_token_stream(),
        };
        folded = quote! { <#node as ::msca::Combine>::combine(#folded, #keep) };
    }
    folded
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

    /// [`expand`] emits the reader, one source-generic `Composite`, the lockstep rebuilder, and
    /// the unfiltered entry point.
    #[test]
    fn expand_emits_reader_and_impls() {
        let code = code();
        assert!(has(&code, "struct RowReader<'a, S>"));
        assert!(has(&code, "::msca::Composite<'a, S> for Row"));
        assert!(has(&code, "Iterator for RowReader<'a, N>"));
        assert!(has(&code, "::msca::Unfiltered<'a> for Row"));
        assert!(has(&code, "impl ::msca::Read for Row"));
    }

    /// [`expand`] emits **one** `Composite` implementation whatever the source shape.
    ///
    /// A second implementation over a fixed source would overlap this one under coherence, and
    /// would mean a filtered read and an unfiltered read assembled their streams by different
    /// routes; the unfiltered path builds the default conjunction and reuses this one instead.
    #[test]
    fn expand_emits_one_composite() {
        let code = code();
        assert_eq!(code.matches("Composite < 'a").count(), 2); // the impl, and the call in it
        assert!(has(
            &code,
            "<Row as ::msca::Composite<'a, ::msca::Conjunct<"
        ));
    }

    /// [`expand`] acquires every column fallibly: each leg is verified against the on-disk column
    /// type by `Query::column`, then builds its per-buffer sources eagerly, so a missing column or
    /// a framing error aborts before any item is yielded.
    #[test]
    fn expand_acquires_streams_fallibly() {
        let code = code();
        assert!(has(&code, "query.column::<u32>(\"a\")?"));
        assert!(has(&code, "::msca::query::Build::build(a,"));
    }

    /// [`expand`] pulls every field stream before inspecting any outcome, and inspects each
    /// outcome exactly once.
    ///
    /// Pulling first is what keeps the columns in lockstep: an error or an absent slot on one
    /// column returns early, and a sibling left unpulled would be a slot behind ever after. One
    /// match then yields both the verdict and the item, so neither is re-derived.
    #[test]
    fn expand_pulls_every_field_before_inspecting_any() {
        let code = code().replace(' ', "");
        let pull = code.rfind("Iterator::next(&mutself.b)").expect("no second pull");
        let inspect = code.find("let(keep_a,a)=matcha").expect("no single-match inspection");
        assert!(pull < inspect);
        assert_eq!(code.matches("=matcha{").count(), 1); // field `a` is matched exactly once
    }

    /// [`expand`] mints one selection and narrows it through every leg before building a stream.
    ///
    /// The selection is settled once, so a buffer cleared by a filter on one leg is never read by
    /// any sibling; each leg then takes a copy of the settled selection to build from.
    #[test]
    fn expand_selects_before_building() {
        let code = code().replace(' ', ""); // generated tokens are space-separated
        let select = code.find("::msca::query::Select::select").expect("no selection");
        let build = code.find("::msca::query::Build::build").expect("no build");
        assert!(select < build);
        assert!(has(&code, "let mut mask = query.mask()"));
    }

    /// [`expand`] builds no combination for a single-field struct, whose whole source is one leg.
    ///
    /// The generated shape is otherwise identical: one source-generic `Composite`, reached by the
    /// unfiltered path exactly as a wider struct is.
    #[test]
    fn expand_single_field_skips_combination() {
        let input: DeriveInput = parse_quote! { struct One { a: u32 } };
        let code = expand(&input).expect("Expansion failed").to_string();
        assert!(has(&code, "::msca::Composite<'a, S> for One"));
        assert!(!has(&code, "::msca::Join::and"));
        assert!(!has(&code, "::msca::Combine"));
    }

    /// [`expand`] propagates the source visibility to the generated reader.
    ///
    /// The reader appears in the public `Read::Src` GAT. A `pub` source must therefore yield a
    /// `pub` reader to avoid leaking a private type through the public interface.
    #[test]
    fn expand_reader_inherits_visibility() {
        let input: DeriveInput = parse_quote! { pub struct Row { a: u32, b: f64 } };
        let code = expand(&input).expect("Expansion failed").to_string();
        assert!(has(&code, "pub struct RowReader<'a, S>"));
    }

    /// Each reader field is a type-erased boxed [`Outcome`] iterator; the opaque column stream
    /// types differ, so a struct field cannot name them directly.
    #[test]
    fn expand_boxes_each_reader_field() {
        let code = code();
        let item = "Item = ::msca::Outcome<u32>";
        let field = format!("a: ::std::boxed::Box<dyn ::core::iter::Iterator<{item}> + 'a>");
        assert!(has(&code, &field));
    }

    /// [`expand`] output parses as valid Rust at every arity.
    ///
    /// One field emits an empty `where` clause and three fields emit an interior node parameter,
    /// so the narrow and wide shapes are checked alongside the shared two-field fixture.
    #[test]
    fn expand_output_parses() {
        let inputs: [DeriveInput; 3] = [
            parse_quote! { struct One { a: u32 } },
            row(),
            parse_quote! { struct Wide { a: u32, b: f64, c: u16 } },
        ];
        inputs.iter().for_each(|input| {
            let expanded = expand(input).expect("Expansion failed");
            syn::parse2::<syn::File>(expanded).expect("Generated code does not parse");
        });
    }

    /// [`expand`] rejects inputs without named fields.
    ///
    /// Field names are required to resolve column streams from the `Query`.
    #[test]
    // TODO → add enum support via variant discriminate (existing support for numerical primitives)
    fn expand_rejects_enum() {
        let input: DeriveInput = parse_quote! { enum Level { Low } };
        expand(&input).expect_err("Unsupported input accepted");
    }
}
