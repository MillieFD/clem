# `clem` Format Specification

Domain-agnostic high-throughput storage for n-dimensional analytical data, written in Rust.

---

### 1. Design Goals

`clem` maximises read and write performance by separating the data lifecycle into two phases:

1. **In memory** accumulator optimised for high-throughput ingestion.
2. **On disk** archive optimised for range-based querying across arbitrary dimensions.

`clem` provides an extensible backend which can be adapted to suit a variety of scientific applications. Implementers
benefit from a minimal high-performance core library which can be further enhanced via domain-specific optimisations.

- **Compact:** Small file size with interleaved segments.
- **Efficient:** Suitable for edge deployment on resource-constrained hardware.
- **Flexible:** Can be adapted to suit a wide variety of applications.
- **Parallel:** First-class support for multiple-producer multiple-consumer workflows.
- **Performant:** Zero-copy random access reads via `mmap`.

To achieve these design goals, `clem` must decouple **logical structure** (types, schemas) from **physical storage**
(segments). This document describes the format design and shows how each goal is met.

---

### 2. Structural Principles

##### 2.1 Columnar Storage

Data is stored in **columnar buffers** to optimise:

- Compression
- SIMD/vectorised access
- Predicate filtering

##### 2.2 Segmented Layout

The file is divided into **self-describing segments** to enable:

- Forward compatibility
- Partial reads & writes
- Crash resilience
- Manifest reconstruction

`clem` is optimised for append-heavy workflows and in situ query reads via `mmap`. Segments are immutable once written.
Whole-segment deletion is permitted but expensive; all downstream segments are moved and the `manifest` is updated.

##### 2.3 Arbitrary Types

`clem` understands **platform-agnostic** primitive types such as `u32` or `f64`. Platform-dependent types such as
`usize` are deliberately omitted to ensure file portability. Additional user-defined types are embedded directly in
the schema using a depth-first cursor-based stateful serializer with no per-field allocation on the hot path.

- Leaf nodes map to contiguous columnar data buffers via index.
- Internal nodes exist purely for navigation & reconstruction.

```text
tree schema → array nodes → buffers
```

For example `struct Outer { foo: Inner, bar: i32 }` and `struct Inner { baz: bool, quux: Option<f64> }` can be encoded
as just three contiguous data buffers and one null bitmap arranged sequentially:

```text
Outer
├─ foo: Inner
│  ├─ baz: bool
│  │  ╭─ Buffer 0 ──────╮
│  │  │ length: u64     │
│  │  │ payload: [u8]   │
│  │  ╰─────────────────╯
│  └─ quux: Option<f64>
│     ╭─ Buffer 1 ──────╮
│     │ length: u64     │
│     │ bitmap: [u8]    │
│     │ payload: [f64]  │
│     ╰─────────────────╯
└─ bar: i32
   ╭─ Buffer 2 ──────╮
   │ length: u64     │
   │ payload: [i32]  │
   ╰─────────────────╯
```

The schema itself can be conceptualised as a `struct` where each column becomes a field with a `name` and `type`.

##### 2.4 Unsized Types

It is not possible to predetermine the disk space required by each instance of an unsized type; there is no guarantee
that one `Vec<T>` contains the same number of elements as another `Vec<T>`. The `clem` type serializer therefore parses
unsized types into:

1. Columnar metadata describing boundaries
2. A contiguous region of elements

This design ensures O(1) random access and avoids per-element pointer chasing. Sequential scans across the contained
elements `[T]` remains linear; leveraging columnar optimisations for SIMD and prefetch.

```text
offsets: [3, 6, 6]
values:  [a, b, c, d, e, f, g, h]
```

The serialized on disk example (above) is deserialized into the memory representation (below). Implementers must specify
which type to use for offset storage based on the number of expected elements. An `Offset` marker trait is implemented
for approved types: u8, u16, u32, u64, u128.

```text
Row 0 → values[..3] → "abc"
Row 1 → values[3..6] → "def"
Row 2 → values[6..6] → "" (empty)
Row 3 → values[6..] → "gh"
```

Nested unsized types serialize into a flattened tree with multiple offset layers. The compositional design preserves
the performance advantages associated with contiguous value storage; namely predictable vectorised traversal. Scanning
performance across the contiguous inner `values` buffer is unaffected by deep nesting. The inner offsets buffer is
aligned in memory order of traversal to improve cache locality during nested iteration and reduce TLB misses.

```text
inner offsets
outer offsets
values
```

##### 2.5 Alignment

`clem` uses **targeted 64-bit alignment** on critical data to ensure:

- SIMD vectorisation
- Memory mapped IO safety
- Cache-line efficiency

Padding is inserted immediately before fields that are accessed directly via `mmap` or processed by SIMD instructions.
Alignment is not enforced for small or non-performance-critical fields to minimise file size.

**Aligned fields**

| Field                         | Reason                                                                          |
|-------------------------------|---------------------------------------------------------------------------------|
| Buffer `payload`              | Primary SIMD target; misalignment silently degrades vectorised reads or faults. |
| Buffer `bitmap`               | Iterated alongside payload; must be cache-line paired with the payload.         |
| Data Segment `offsets: [u64]` | Cast directly from `mmap`; misalignment is undefined behaviour.                 |
| Unsized `offsets` buffer      | Read directly during boundary lookup; 64-bit alignment improves access safety.  |
| Unsized `values` buffer       | Contiguous hot-path payload; 64-bit alignment benefits traversal efficiency.    |

Exactly one padding region is inserted per buffer; between the end of the header and the start of the first aligned
field (`bitmap` if present, otherwise `payload`).

**Unaligned fields**

| Field                                        | Reason                                                            |
|----------------------------------------------|-------------------------------------------------------------------|
| Segment Header `variant: u8`                 | Read once per segment during discovery; never vectorised.         |
| Segment Header `schema: u64` & `length: u64` | Fixed-width `u64` copied into owned values during header parsing. |
| File Header `version: u8` and `magic: [u8]`  | Read once when file openned; zero benefit from alignment.         |
| Schema Segment payload                       | Deserialised into owned type tree; not accessed on the hot path.  |
| Manifest (CBOR)                              | Variable-length text formats deserialised into owned structures.  |

Byte order is little-endian throughout.

##### 2.6 Lazy Partial Reads

On disk data is read lazily, being represented via a minimal `Sector` struct prior to file IO. This design ensures:

- **Fast random access:** Readers `seek` directly to the pertinent file region.
- **Memory efficient:** Readers `take` exactly the required number of bytes instead of loading the entire file.

Passing a small `Sector` instance can reduce overhead compared to passing an in-memory data buffer. Alternative read
functions that return the underlying `Sector` without file IO are provided so that implementers can work directly with
on disk storage regions.

```rust
/// A contiguous byte range within the file.
#[derive(Debug, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub struct Sector {
    /// Byte offset to the start of the segment.
    offset: SeekFrom::start,
    /// Length in bytes.
    length: usize,
}
```

`Sector` implements several traits for convenience:

1. `Ord` and `PartialOrd` compare offsets. One sector is considered `less` than another which starts closer to EOF.
2. `Eq` and `PartialEq` compare offsets and lengths. Two sectors are considered `equal` only if they start at the same
   offset and extend for the same length i.e. represent identical data.
3. `IntoIterator` allows implementers to stream bytes from the file.

Sectors enforce the immutability of underlying on disk data. Implementers are advised to `read` data into an owned
in-memory collection when mutability is required e.g. downstream data processing.

---

### 3. Segment Types

Each `Segment` describes a file region written to disk, defined by a starting `offset` and `length` in bytes. In
addition to conventional `data` segments – which encode columnar data buffers – format extensibility is achieved via
segment variants, each identified via a `variant: u8` ID in the segment header. A `length` field allows sequential
readers to skip to the next segment (no segment footer required).

##### 3.1 Schema Segments

To construct a schema, users must first define a `struct` which implements `serde::Serialize`. Each field becomes a
column with a `name` and `type`. Each instance of the `struct` represents a row. This design moves schema validity
checks to compile time by leveraging Rust's type safety, improving data ingestion speed by eliminating costly runtime
schema checks.

```text
Schema Segment
├─ Header
│  ├─ variant: u8
│  └─ length: u64
└─ Payload
```

Readers can directly query data from arbitrary named fields – without reconstructing a type instance – by reading the
corresponding columnar data buffer. Each schema segment encodes **one** schema and each `clem` file requires at least
**one** schema segment. Multimodality and schema evolution are achieved by appending additional segments.

##### 3.2 Data Segments

Each data segment is associated with **one** schema segment with an offset – for random access reads – encoded via the
`schema: u64` header field. This association is principally included for data integrity and crash recovery; the
optmised read path pre-filters data segments by schema using the `manifest`.

```text
Data Segment
├─ Header
│  ├─ variant: u8
│  ├─ schema: u64
│  ├─ length: u64
│  └─ offsets: [u64]
├─ Buffer 0
│  ├─ length: u64
│  └─ payload: [u8]
├─ Buffer 1
│  ├─ length: u64
│  ├─ bitmap: [u8]
│  └─ payload: [u8]
⋮
└─ Buffer N
```

The schema maps each leaf node to a contiguous data buffer. The offset of each buffer is read from the `offsets: [u64]`
array using the column index. All columns must have an equal number of rows. Buffer payload deserialization is informed
by the column type described by the schema. Where the schema indicates optional values, the buffer payload is preceded
by a packed nullable bitmap.

### 4 Dictionaries

The storage cost for large types with repetitive values can be amortised using a dictionary, which is implemented as
user-friendly abstraction over the underlying schema and data segments coordinated via the manifest.

The `Dataset` (exclusive file handle) contains a `dictionaries: BTreeMap` field which is parsed from the manifest when
the file is first opened via `Dataset::open` or `Dataset::new`. The `BTreeMap` is used to look up dictionaries by name.
Note that a platform-agnostic `BTreeMap` is used to ensure determinism; the order of elements within the map does not
necessarily represent the physical order on disk.

```rust
impl Dataset {
    /// Returns an exclusive reference to the specified [`Dictionary`]. Initialises a new [`Dictionary`] with the
    /// specified `key: K` and `value: V` types if no entry exists for the specified `name: S`.
    ///
    /// ### Errors
    ///
    /// Returns an [`Error`] if a dictionary with the specified `name` already exists using a different `V` type.
    pub async fn dictionary<K, V>(&mut self, name: impl Display) -> Result<RwLock<Dictionary<V>>, Error>
    where
        K: Sized + Ord,
        V: serde::Serialize,
    { ... }
}
```

The `Dataset::dictionary` function is used to create and retrieve named dictionaries wrapped in an asyncronous `RwLock`.
Callers must `await` due to file IO on the creation branch; existing dictionaries return `Poll::Ready` immediatley.
The `Dictionary` supports multiple simultaneous readers or a single exclusive writer to prevent key collision; callers
can choose to `await` mutable or immutable access.

##### 4.1 Dictionary Schema

A dictionary inherently requires two opposing access patterns:

1. **Keys** optimised for search performance → columnar
2. **Values** optimised for extraction and reconstruction → row-oriented

The dictionary is built using standard schema and data segments. The user-defined `K` and `V` types are wrapped in a
parent struct `D { key: K, value: [V] }` which is then serialized into a schema segment. Values are wrapped in a
collection type such as `Vec` to enable contiguous row-orientated storage.

```text
user types → dictionary type → schema segment + [data segments]
```

Dictionaries are append-only and grow via additional data segments. The `Dictionary` struct therefore acts as an in
memory accumulator plus additional `get` functionality. A `values` blob stores each `value: V` contiguously. An
`offsets` buffer identifies the region for each value that is then deserialized using to the schema downstream of `V`.

```text
[keys] [offsets] [ values [value 0 [field 0] ... [field N] ] ... [value N] ]
```

##### 4.2 Dictionary Entries

Entries are stored via ordinary data segments referencing the dictionary schema. Each data segment row represents a
single key-value pair.

```rust
impl<K, V> Dictionary<K, V> { pub fn push(&mut self, key: K, value: V) -> Result<K, Error> { ... } }
```

Users can `push` new entries to the internal `pending: Vec<V>` field which is written to disk as an ordinary data
segment on `drop`. The dictionary ensures key uniqueness by returning an error from `push` if the specified key is
already present. The `K: Ord` trait bound enables the manifest to store key statistics such as `min` and `max` which
improve search performance across multiple data segments. Additional manifest column statistics – and corresponding
trait bounds – may be added in future versions.

Value retrieval follows a four-step process:

1. Search for the specified key (on disk and pending).
2. Get the corresponding offsets.
3. Read the identified region from `values` blob.
4. Deserialize into a `V` instance.

##### 4.3 Index Dictionaries

A specialised `Index` dictionary implementation is provided for entries keyed by insertion order. The key is
automatically incremented for each `push` call; creating a new index initialises the key at zero, whereas opening an
existing index eagerly reads the max existing key from the manifest. An index is recommended for dense ordered data
where position is the only required identifier.

```text
push(value 0) → key 0
push(value 1) → key 1
push(value 2) → key 2
```

The `push` function is simplified compared to a generic dictionary and returns the inserted key without `Result` as the
index prevents key collision. Implementers can specify the key numeric type based on the number of expected elements.

```rust
impl<K, V> Index<K, V>
where
    K: crate::IndexKey, // Marker trait for approved key types: u8, u16, u32, u64, u128
    V: serde::Serialize,
{
    pub fn push(&mut self, value: V) -> K { ... }
}
```

The `Index` is implemented as a standard dictionary with one notable optimisation: the on disk `keys` column is ommitted
as values are searched by index. The manifest stores `count` for each data segment which enables direct access via index
arithmetic; if data segment 0 contains `100` values and data segment 1 contains a further `45` values, entry number
`110` is located at index `10` in data segment 0.

---

### 5. Manifest

A `manifest` footer lists file segments by type. Data segments are grouped by schema alongside segment-level
statistics e.g. min and max values. The `manifest` acts like the index of a book to enhance:

- Segment discovery
- Random access
- Predicate pruning

The manifest is encoded as **CBOR** with definite-length text maps to enable schema and column access by name. A
`metadata` key is included when user-specified file-level metadata is present. The manifest is moved and updated when
new segments are added.

```text
Manifest
├─ metadata (optional)
├─ dictionaries: BTreeMap (optional)
└─ schemas: BTreeMap
   ├─ <schema-name>
   │  ├─ sector: Sector
   │  └─ columns: BTreeMap
   │     ├─ <column-name>
   │     │  └─ buffers: [Buffer]
   │     │     ├─ sector: Sector
   │     │     ├─ count: u32
   │     │     ├─ min: T
   │     │     └─ max: T
   │     ⋮
   │     └─ <final-column>
   ⋮
   └─ <final-schema>
```

Schema lookup by name returns the corresponding schema segment and a map of all schema columns. A `BTreeMap<String,
Schema>` sorted in lexicographic order is used to ensure a fully deterministic layout regardless of insertion order.

```text
manifest["schema_name"] → Schema { segment: Segment, columns: BTreeMap<String, Column> }
```

Column lookup by name returns the corresponding collection of buffers across all on disk data segments.

```text
manifest["schema_name"]["column_name"] → [Buffer]
```

Each `Buffer` contains a `sector: Sector` alongside data statistics such as `min` and `max` for predicate pruning.

##### 5.1 Metadata

Implementers can use the optional free-form `metadata.toml` to attach file-level domain-specific information such as:

- Date and time
- Experimental parameters
- Provenance

If a metadata section is included in the file, a corresponding `length` and `offset` are described in the `manifest`.

### 6. Lifecycle

`clem` maximises read and write performance by separating the data lifecycle into two phases:

1. **In memory** accumulator optimised for high-throughput ingestion.
2. **On disk** archive optimised for range-based querying across arbitrary dimensions.

##### 6.1 In Memory Accumulator

Data is initially written to an **in memory** accumulator optimised for high-throughput ingestion. The `pub struct 
Accumulator` is generic over any type `R` that implements `serde::Serialize`. The accumulator implements
`serde::Serializer` to serialized ingested row-wise data into columnar `Vec` buffers which can be written to disk.

The accumulator includes a number of public functions:

```rust
impl<R> Accumulator<R>
where
    R: serde::Serialize
{
    /// Append a row-wise record to the internal columnar [`Vec`] buffers.
    pub fn push(&self, record: R) { ... }

    /// Extends the accumulator buffers with the contents of an iterator.
    pub fn extend<I>(&self, iterator: I) where I: IntoIterator<Item = R> { ... }

    /// Builds a schema segment for type `R` and writes to disk. Returns the written [`Sector`] is successful, which
    /// is also cached to the lazily initialised `schema: Sector` field.
    pub async fn schema(&self) -> Result<Sector, Error> { ... }

    /// Writes a new data segment to disk. Returns the written [`Sector`] if successful.
    ///
    /// The internal columnar buffers are consumed and reinitialised with [`Vec::new`] ready for further data ingestion.
    ///
    /// [`Write`] uses the lazily initialised `schema: Sector` field which calls [`Self::schema`] on first access,
    /// hence ensuring that a schema segment is always written to disk before any dependent data segments.
    pub async fn write(&self) -> Result<Sector, Error> { ... }

    /// Reinitialise the columnar data buffers without writing data to disk. All accumulated data is permanently lost.
    pub fn discard(&self) { ... }
}
```

Users can explicitly write data to disk using the `write` function. Data is automatically written to disk on `drop` if
the buffers are not empty, or if `count` reaches `u32::MAX` due to size limitation in the manifest.  

##### 6.2 Parallel Accumulators

Accumulators are thread-local. Multi-producer workloads build segments independently via separate in memory accumulator
instances spawned from the same dataset. Users can spawn an arbitrary number of accumulators via `Dataset::accumulator`.

```rust
impl Dataset { pub fn accumulator(&self) -> Result<Accumulator, Error> { ... } }
```

The `Dataset` (exclusive file handle) coordinates access to the underlying file; preventing multiple accumulators from
writing to disk simultaneously. All interactions with the underlying file and global lock are implemented asynchronously
via `smol`.

##### 6.3 On Disc File

```text
File
├─ Header
│  ├─ magic: [u8]
│  ├─ version: u8
│  └─ manifest
│     ├─ offset: u64
│     └─ length: u64
├─ Segment 0
⋮
├─ Segment N
├─ Manifest
└─ Metadata
```

##### 6.4 Write Cycle

Appending a new segment to the file – regardless of type – requires four steps:

1. Manifest and metadata (if present) read into memory.
2. New segment written at EOF, overwriting the previous manifest and metadata.
3. Manifest updated with additional segment information.
4. In memory manifest and metadata written to disk at new EOF.
