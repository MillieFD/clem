/*
Project: clem
GitHub: https://github.com/MillieFD/clem

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! Type graph logic for schema generation and structural matching.
//!
//! ---
//!
//! ### Arbitrary Type Encoding
//!
//! `clem` understands **platform-agnostic** primitive types such as `u32` or `f64`.
//! Platform-dependent types such as `usize` are deliberately omitted to ensure file portability.
//! Additional user-defined types are embedded directly in the schema using a depth-first
//! cursor-based stateful serializer with no per-field allocation on the hot path.
//!
//! - **Leaf nodes** map to contiguous columnar data buffers via index.
//! - **Internal nodes** exist purely for navigation and reconstruction.
//!
//! For example, `struct Outer { foo: Inner, bar: i32 }` and `struct Inner { baz: bool, quux:
//! Option<f64> }` can be encoded as just three contiguous data buffers and one packed null bitmap
//! arranged sequentially:
//!
//! ```text
//! Outer
//! ├─ foo: Inner
//! │  ├─ baz: bool
//! │  │  ╭─ Buffer 0 ─────────╮
//! │  │  │ length: NonZeroU64 │
//! │  │  │ payload: [u8]      │
//! │  │  ╰────────────────────╯
//! │  └─ quux: Option<f64>
//! │     ╭─ Buffer 1 ─────────╮
//! │     │ length: NonZeroU64 │
//! │     │ bitmap: [u8]       │
//! │     │ payload: [f64]     │
//! │     ╰────────────────────╯
//! └─ bar: i32
//!    ╭─ Buffer 2 ─────────╮
//!    │ length: NonZeroU64 │
//!    │ payload: [i32]     │
//!    ╰────────────────────╯
//! ```
//!
//! The schema itself can be conceptualised as a `struct` where each column becomes a field with a
//! `name` and `type`.
//!
//! ### Unsized Types
//!
//! It is not possible to predetermine the disk space required by each instance of an unsized type;
//! there is no guarantee that one `Vec<T>` contains the same number of elements as another
//! `Vec<T>`. The `clem` type serializer therefore parses unsized types into:
//!
//! 1. Columnar metadata describing boundaries
//! 2. A contiguous region of elements
//!
//! This design ensures **O(1) random access** and avoids per-element pointer chasing. Sequential
//! scans across the contained elements `[T]` remain linear; leveraging columnar optimisations for
//! SIMD and prefetch.
//!
//! ```text
//! offsets: [3, 6, 6]
//! values:  [a, b, c, d, e, f, g, h]
//! ```
//!
//! The serialized on-disk example above is deserialized into the memory representation below.
//! Implementers must specify which type to use for offset storage based on the number of expected
//! elements. A `NonZeroUInt` marker trait is implemented for approved types. The `offsets` buffer
//! can simultaneously encode nullability by leveraging niche-optimisation on non-zero types.
//!
//! ```text
//! Row 0 → values[..3] → "abc"
//! Row 1 → values[3..6] → "def"
//! Row 2 → values[6..6] → "" (empty)
//! Row 3 → values[6..] → "gh"
//! ```
//!
//! Nested unsized types serialize into a flattened graph with **multiple offset layers**. This
//! composable design preserves the performance advantages associated with contiguous value storage;
//! namely predictable vectorised traversal. Scanning performance across the contiguous inner
//! `values` buffer is unaffected by deep nesting. The inner offsets buffer is aligned in memory
//! order of traversal to improve cache locality during nested iteration and reduce TLB misses.
//!
//! ```text
//! inner offsets
//! outer offsets
//! values
//! ```
//!
//! ### Schema Segments
//!
//! To construct a [`Schema`], users must first define a `struct` which implements
//! `serde::Serialize`. Each field becomes a column with a `name` and `type`. Each instance of the
//! struct represents a row. This design moves schema validity checks to compile time by leveraging
//! Rust's type safety, improving data ingestion speed by eliminating costly runtime schema checks.
//! The type tree is serialized into CBOR; encoding the `name` and `type` of all internal and leaf
//! nodes. The user schema struct (root node) defines the schema name which is encoded in the type
//! tree and written to the file manifest `schemas: BTreeMap` to enable schema retrieval by name.
//!
//! ```text
//! Schema Segment
//! ├─ Header
//! │  ├─ variant: u8
//! │  └─ length: NonZeroU64
//! └─ Payload: CBOR
//! ```
//!
//! Readers can directly query data from arbitrary named fields – without reconstructing a type
//! instance – by reading the corresponding columnar data buffer. Each schema segment encodes
//! **one** schema and each `clem` file requires at least **one** schema segment. Multimodality and
//! schema evolution are achieved by appending additional segments.
//!
//! ### 6.3 Schema Validation
//!
//! `Dataset::stream` compares the type graph root node name for `R` against the manifest
//! `schemas: BTreeMap`; initialising a new stream with `schema: R` if no entry exists for the
//! specified name or returning an error if the `R` type graph does not exactly match the existing
//! schema structure.
//!
//! - An exact structural match is required for read and write validity via [`Stream`].
//! - Subset-matches (projections) enable read-only access via [`SubStream`].
//!
//! This design ensures schema verification is performed exactly once. Stream read and write
//! operations can then proceed fearlessly without per-request runtime checks on the hot path.
//! Cloning an existing `Stream<R>` bypasses schema validation. Multi-consumer workloads should
//! therefore prefer cloning a validated stream instead of calling `Dataset::stream` repeatedly.
//! Implementers are encouraged to export canonical types for convenience, removing the need for
//! users to reconstruct schema types manually.

