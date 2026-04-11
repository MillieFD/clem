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
- **Performant:** Efficient random access reads.

To achieve these design goals, `clem` must decouple **logical structure** (types and schemas) from **physical storage**
(segments). This document describes the format design and shows how each goal is met.

---

### 2. Structural Principles

##### 2.1 Columnar Storage

Data is stored in **columnar buffers** to optimise:

- Compression
- SIMD vectorised access
- Predicate filtering

##### 2.2 Segmented Layout

The file is divided into **self-describing segments** to enable:

- Forward compatibility
- Partial reads & writes
- Crash resilience
- Manifest reconstruction

`clem` is optimised for append-heavy workflows and range-based query reads. Segments are immutable once written.
Whole-segment deletion is permitted but expensive; all downstream segments are moved and the `manifest` is updated.

##### 2.3 Arbitrary Types

`clem` understands **platform-agnostic** primitive types such as `u32` or `f64`. Platform-dependent types such as
`usize` are deliberately omitted to ensure file portability. Additional user-defined types are embedded directly in
the schema using a depth-first cursor-based stateful serializer with no per-field allocation on the hot path.

- **Leaf nodes** map to contiguous columnar data buffers via index.
- **Internal nodes** exist purely for navigation & reconstruction.

```text
tree schema → array nodes → buffers
```

For example `struct Outer { foo: Inner, bar: i32 }` and `struct Inner { baz: bool, quux: Option<f64> }` can be encoded
as just three contiguous data buffers and one packed null bitmap arranged sequentially:

```text
Outer
├─ foo: Inner
│  ├─ baz: bool
│  │  ╭─ Buffer 0 ─────────╮
│  │  │ length: NonZeroU64 │
│  │  │ payload: [u8]      │
│  │  ╰────────────────────╯
│  └─ quux: Option<f64>
│     ╭─ Buffer 1 ─────────╮
│     │ length: NonZeroU64 │
│     │ bitmap: [u8]       │
│     │ payload: [f64]     │
│     ╰────────────────────╯
└─ bar: i32
   ╭─ Buffer 2 ─────────╮
   │ length: NonZeroU64 │
   │ payload: [i32]     │
   ╰────────────────────╯
```

The schema itself can be conceptualised as a `struct` where each column becomes a field with a `name` and `type`.

##### 2.4 Unsized Types

It is not possible to predetermine the disk space required by each instance of an unsized type; there is no guarantee
that one `Vec<T>` contains the same number of elements as another `Vec<T>`. The `clem` type serializer therefore parses
unsized types into:

1. Columnar metadata describing boundaries
2. A contiguous region of elements

This design ensures **O(1) random access** and avoids per-element pointer chasing. Sequential scans across the contained
elements `[T]` remains linear; leveraging columnar optimisations for SIMD and prefetch.

```text
offsets: [3, 6, 6]
values:  [a, b, c, d, e, f, g, h]
```

The serialized on disk example (above) is deserialized into the memory representation (below). Implementers must specify
which type to use for offset storage based on the number of expected elements. A `NonZeroUnsigned` marker trait is
implemented for approved types. The `offsets` buffer can simultaneously encode nullability by leveraging
niche-optimisation on non-zero types.

```text
Row 0 → values[..3] → "abc"
Row 1 → values[3..6] → "def"
Row 2 → values[6..6] → "" (empty)
Row 3 → values[6..] → "gh"
```

Nested unsized types serialize into a flattened tree with **multiple offset layers**. This composable design preserves
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

Padding is inserted immediately before fields that require aligned access or are processed by SIMD instructions.
Alignment is not enforced for small or non-performance-critical fields to minimise file size.

**Aligned fields**

| Field                  | Reason                                                                          |
|------------------------|---------------------------------------------------------------------------------|
| Buffer `payload`       | Primary SIMD target; misalignment silently degrades vectorised reads or faults. |
| Buffer `bitmap`        | Iterated alongside payload; must be cache-line paired with the payload.         |
| Data Segment `offsets` | Cast directly from a memory-mapped region; misalignment is undefined behaviour.    |
| Unsized Type `offsets` | Read directly during boundary lookup; 64-bit alignment improves access safety.  |
| Unsized Type `values`  | Contiguous hot-path payload; 64-bit alignment benefits traversal efficiency.    |

Exactly one padding region is inserted per buffer; between the end of the header and the start of the first aligned
field (`bitmap` if present, otherwise `payload`).

**Unaligned fields**

| Field                                | Reason                                                                   |
|--------------------------------------|--------------------------------------------------------------------------|
| Segment Header `variant`             | Read once per segment during discovery; never vectorised.                |
| Segment Header `schema` and `length` | Fixed-width `NonZeroU64` copied into owned values during header parsing. |
| File Header `version` and `magic`    | Read once when file opened; zero benefit from alignment.                 |
| Schema Segment payload               | Deserialised into owned type tree; not accessed on the hot path.         |
| Manifest (CBOR)                      | Variable-length text formats deserialised into owned structures.         |

Byte order is little-endian throughout.

##### 2.6 Lazy Partial Reads

On disk data is read lazily, being represented via a minimal `Segment` struct prior to file IO. This design ensures:

- **Fast random access:** Readers `seek` directly to the pertinent file region.
- **Memory efficient:** Readers `take` exactly the required number of bytes instead of loading the entire file.

Passing small `Segment` instances can reduce overhead compared to passing owned data buffers.

```rust
/// A contiguous byte range within the file.
#[derive(Debug, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub struct Segment {
    /// Byte offset to the start of the region.
    offset: SeekFrom,
    /// Length in bytes.
    length: usize,
}
```

`Segment` implements several traits for convenience:

1. `Ord` and `PartialOrd` compare offsets. One segment is considered `less` than another which starts closer to EOF.
2. `Eq` and `PartialEq` compare offsets and lengths. Two segments are considered `equal` only if they start at the same
   offset and extend for the same length i.e. represent identical data.

Segments enforce the immutability of underlying on disk data. Implementers are advised to `collect` data into an owned
type when mutability is required e.g. for downstream data processing.

---

### 3. Segment Types

Each `Segment` describes a file region written to disk, defined by a starting `offset` and `length` in bytes. In
addition to conventional `data` segments – which encode columnar data buffers – format extensibility is achieved via
segment variants, each identified via a `variant: u8` ID in the segment header. A `length` field allows sequential
readers to skip to the next segment (no segment footer required).

##### 3.1 Schema Segments

To construct a schema, users must first define a `struct` which implements `serde::Serialize`. Each field becomes a
column with a `name` and `type`. Each instance of the struct represents a row. This design moves schema validity
checks to compile time by leveraging Rust's type safety, improving data ingestion speed by eliminating costly runtime
schema checks. The type tree is serialized into CBOR; encoding the `name` and `type` of all internal and leaf nodes.
The user schema struct (root node) defines the schema name which is encoded in the type tree and written to the file
manifest `schemas: BTreeMap` to enable schema retrieval by name.

```text
Schema Segment
├─ Header
│  ├─ variant: u8
│  └─ length: NonZeroU64
└─ Payload: CBOR
```

Readers can directly query data from arbitrary named fields – without reconstructing a type instance – by reading the
corresponding columnar data buffer. Each schema segment encodes **one** schema and each `clem` file requires at least
**one** schema segment. Multimodality and schema evolution are achieved by appending additional segments.

##### 3.2 Data Segments

Each data segment is associated with **one** schema via the schema segment offset – for random access reads – encoded
in the `schema: NonZeroU64` header field. This association is principally included for data integrity and crash
recovery; the optimised read path pre-filters data segments by schema using the `manifest`.

```text
Data Segment
├─ Header
│  ├─ variant: u8
│  ├─ schema: NonZeroU64
│  ├─ length: NonZeroU64
│  ├─ count: NonZeroU64
│  └─ offsets: [Option<NonZeroU64>]
├─ Buffer 0
│  ├─ length: NonZeroU64
│  └─ payload: [u8]
├─ Buffer 1
│  ├─ length: NonZeroU64
│  ├─ bitmap: [u8]
│  └─ payload: [u8]
⋮
└─ Buffer N
```

The schema maps each leaf node to a contiguous data buffer where `offsets[i]...offsets[i+1]` describes the region
occupied by buffer `i`. The `offsets` buffer simultaneously acts as a null bitmap by leveraging niche-optimisation to
indicate columns which are excluded to improve space efficiency if:

1. Buffer type is `Option<T>` and contains all `None` values.
2. Buffer type is `bool` and contains all `false` values.
3. Buffer type is an integer and contains all `zero` values.

All columns contain an equal number of rows indicated by `count` in the segment header. Buffer payload deserialization
is informed by each column type described in the schema. Where the schema indicates optional values, the buffer payload
is preceded by a packed nullable bitmap.

---

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
    pub async fn dictionary<K, V>(&mut self, name: impl Display) -> Result<RwLock<Dictionary<K, V>>, Error>
    where
        K: Serialize + Sized + Ord,
        V: Serialize,
    { ... }
}
```

The `Dataset::dictionary` function is used to create and retrieve named dictionaries wrapped in an async `RwLock`.
Callers must `await` due to file IO on the creation branch; existing dictionaries return `Poll::Ready` immediately.
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
    K: Serialize + crate::NonZeroUnsigned, // Marker trait for approved key types
    V: Serialize,
{
    pub fn push(&mut self, value: V) -> K { ... }
}
```

The `Index` is implemented as a standard dictionary with one notable optimisation: the on disk `keys` column is omitted
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
   │  ├─ segment: Segment
   │  └─ columns: BTreeMap
   │     ├─ <column-name>
   │     │  └─ buffers: [Buffer]
   │     │     ├─ segment: Segment
   │     │     ├─ count: NonZeroU32
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

Each `Buffer` contains a `segment: Segment` alongside data statistics such as `min` and `max` for predicate pruning.

##### 5.1 Metadata

Implementers can use the optional free-form `metadata.toml` to attach file-level domain-specific information such as:

- Date and time
- Experimental parameters
- Provenance

If a metadata section is included in the file, a corresponding `length` and `offset` are described in the `manifest`.
The core library includes a read and write surface, but implementers must include their own metadata parsing and
validation logic.

##### 5.2 Manifest Rebuild

The file manifest provides an optimised read path for random access; however, manifest corruption can occur following a
crash during the write cycle. As each segment is self-describing, a sequential reader can rebuild the manifest from
scratch. This behaviour is triggered automatically during `Dataset::open` if manifest corruption is detected, or users
can call `Manifest::rebuild` explicitly.

---

### 6. Lifecycle

`clem` maximises read and write performance by separating the data lifecycle into two phases:

1. **In memory** accumulator optimised for high-throughput ingestion.
2. **On disk** archive optimised for range-based querying across arbitrary dimensions.

##### 6.1 In Memory Accumulation

Data is initially written to an **in memory** accumulator optimised for high-throughput ingestion. The `pub struct 
Stream` is generic over any type `R` that implements `serde::Serialize` and `serde::Deserialize`. The stream implements
`serde::Serializer` to serialize ingested row-orientated data into columnar buffers which are then written to disk.

```rust
impl<R> Stream<R>
where
    R: Serialize + Deserialize
{
    /// Append a row-orientated record to the internal columnar [`Vec`] buffers.
    pub fn push(&mut self, record: R) { ... }

    /// Extends the stream buffers with the contents of an iterator.
    pub fn extend<I>(&mut self, iterator: I) where I: IntoIterator<Item = R> { ... }

    /// Builds a schema segment for type `R` and writes to disk. Returns the written [`Segment`] if successful, which
    /// is also cached to the lazily initialised `schema: Segment` field.
    pub async fn schema(&mut self) -> Result<Segment, Error> { ... }

    /// Writes a new data segment to disk. Returns the written [`Segment`] if successful.
    ///
    /// The internal columnar buffers are consumed and reinitialised with [`Vec::new`] ready for further data ingestion.
    ///
    /// [`Write`] uses the lazily initialised `schema: Segment` field which calls [`Self::schema`] on first access,
    /// hence ensuring that a schema segment is always written to disk before any dependent data segments.
    pub async fn write(&mut self) -> Result<Segment, Error> { ... }

    /// Reinitialise the columnar data buffers without writing data to disk. All accumulated data is permanently lost.
    pub fn discard(&mut self) { ... }
}
```

Users can explicitly write data to disk using the `write` function. Data is automatically written to disk on `drop` if
the buffers are not empty, or if `count` reaches `u64::MAX` to prevent counter overflow.

##### 6.2 Parallel Streams

Streams are thread-local. Multi-producer workloads build segments independently via separate in memory streams spawned
from the same dataset. Users can spawn an arbitrary number of streams via `Dataset::stream`.

```rust
impl Dataset { pub fn stream<R>(&mut self) -> Result<Stream<R>, Error> { ... } }
```

The `Dataset` (exclusive file handle) coordinates file access; preventing multiple streams from writing to disk
simultaneously. All interactions with the underlying file and global lock are implemented asynchronously.

##### 6.3 Schema Validation

`Dataset::stream` compares the `R` type tree root node name against the manifest `schemas: BTreeMap`; initialising a new
stream with `schema: R` if no entry exists for the specified name or returning an error if the `R` type tree does not
exactly match the existing schema structure.

- An exact structural match is required for read and write validity.
- Subset-matches (projections) are rejected; use `Dataset::substream` instead.

This design ensures schema verification is performed exactly once. Stream read and write operations can then proceed
fearlessly without per-request runtime checks on the hot path. Cloning an existing `Stream<R>` bypasses schema
validation. Multi-consumer workloads should therefore prefer cloning a validated stream instead of calling
`Dataset::table` repeatedly. Implementers are encouraged to export canonical types for convenience, removing the need
for users to reconstruct schema types manually.

```rust
impl Dataset { pub fn substream<T>(&self, schema: String) -> Result<SubStream<T>, Error> { ... } }
```

Users can define lightweight read-only views without pulling unnecessary columns. The dataset searches the specified
schema for a structural subset-match (projection) against the `T` type tree, returning an error if no match is found.
Type and field names are not considered; matching is purely structural to grant users additional flexibility.

##### 6.3 On Disk File

The file header begins with a magic byte sequence used to identify the file type. Implementers must reject incorrect
magic byte sequences. Implementers may prepend their own file header – e.g. to indicate a specific file type built atop
`clem` with a canonical schema – but must remove the prepended data before passing to the underlying reader.

```text
File
├─ Header
│  ├─ magic: [u8; 4] // b"clem"
│  ├─ version: u8
│  ├─ tail: NonZeroU64
│  └─ manifest
│     ├─ offset: NonZeroU64
│     └─ length: NonZeroU64
├─ Segment 0
⋮
├─ Segment N
├─ Empty (optional)
├─ Manifest
└─ Metadata
```

A major version number is embedded in the file header to indicate breaking changes in the format specification. Forwards
and backwards compatibility across version numbers is not guaranteed. Implementers must reject any file with an
unrecognised version number.

```text
[Header] [Segment 0] ... [Segment N] ... [Manifest] [Metadata]
                               tail ↑    ↑ offset
```

The `tail: NonZeroU64` field records the byte offset immediately following the final committed segment. New segments are
always appended from `tail`, not from EOF. An empty region may exist between `tail` and the start of the manifest when
appending segments that are shorter than the combined manifest and metadata. This empty region is filled during the next
write cycle.

##### 6.4 Write Cycle

Let `m` denote the combined byte length of the existing manifest and metadata (if present). Let `s` denote the byte
length of the incoming segment. The write cycle exploits the relationship between `s` and `m` to guarantee that the
previous manifest is never overwritten before the new manifest pointer is committed to the file header.

Appending a new segment to the file – regardless of type – requires four phases:

**Phase 1:** Write the new manifest at EOF.

The `Dataset` contains a `manifest: RwLock<Manifest>` field which is lazily initialised from disk on first access by:

1. Reading the file header to determine manifest `offset` and `length`.
2. Deserializing the on disk CBOR manifest into an in memory `Manifest` instance.

The exiting in memory manifest is updated to include the incoming segment. The new manifest and metadata (if present)
are written to a postition relative to `tail` depending on `s` and `m`:

- `s > m` → The new segment is larger than the combined existing manifest and metadata. The new manifest is written
  starting `s` bytes after `tail` to reserve the exact disk space required by the incomming segment. This introduces a
  transient empty region between the previous EOF and the new manifest offset.

- `s == m` → The new segment exactly fills the space occupied by the old manifest and metadata. The new manifest is
  written immediately following the prefious EOF with no empty region.

- `s < m` → The new segment is smaller than the combined existing manifest and metadata. The new manifest is written
  immediately following the prefious EOF with no empty region, leaving an unreferenced trailing region from `tail + s`
  to the new manifest offset. This trailing region lies beyond `tail` and is therefore invisible to readers; it is
  naturally overwritten in the next write cycle.

At the end of step 1, the file contains two manifests. The old manifest remains authoritative because the file header
has not yet been updated. A crash in phase 1 leaves the file contents intact. The new manifest are unreferenced and will
be overwritten in the next write cycle as `tail` remains unmoved.

```text
                                          Reserved for Incoming Segment
                                    ├───────────────────────────────────────┤
[Header] [Segment 0] ... [Segment N] ... [Prev Manifest] [Prev Metadata] ... [New Manifest] [New Metadata]
                               tail ↑   ↑ offset
```

**Phase 2:** Update the file header manifest pointer.

The `manifest.offset` and `manifest.length` fields in the file header are overwritten to point to the new manifest.
The newly authoritative manifest references a (currently unwritten) segment after `tail` which will be detected during
the next `open` call.

```text
                                          Reserved for Incoming Segment
                                    ├───────────────────────────────────────┤
[Header] [Segment 0] ... [Segment N] ... [Prev Manifest] [Prev Metadata] ... [New Manifest] [New Metadata]
                               tail ↑                                       ↑ offset
```

**Phase 3:** Write the incoming segment.

The incoming segment is written starting from `tail` and overwriting the old manifest and any empty regions if present.
Crash detection and recovery is identical to phase 2.

```text
[Header] [Segment 0] ... [Segment N] [New Segment] ... [New Manifest] [New Metadata]
                               tail ↑                 ↑ offset
```

**Phase 4:** Update the file header tail pointer.

The `tail` field is advanced to `tail + s`, pointing immediately after the end of the newly written segment. The write
cycle is complete with `manifest.offset <= tail` and the manifest correctly indexing all committed segments.

```text
[Header] [Segment 0] ... [New Segment] ... [New Manifest] [New Metadata]
                                 tail ↑   ↑ offset
```

##### 6.5 Read Cycle

The read cycle is built upon two complementary principles:

1. **Manifest-driven random access** with predicate pruning to minimise unnecessary IO.
2. **Granular cooperative locking** to operate **multiple** parallel readers concurrently alongside up to **one** active
   writer without contention.

Reading data from the file – across an arbitrary number of segments – requires up to three phases:

**Phase 1: Manifest Resolution**

The `Dataset` contains a `manifest: RwLock<Manifest>` field which is lazily initialised from disk on first access by:

1. Reading the file header to determine manifest `offset` and `length`.
2. Deserializing the on disk CBOR manifest into an in memory `Manifest` instance.
3. Downgrading access to read guard to minimise contention.

All `write` operations update the manifest in memory before commiting to disk. All subsequent `read` operations acquire
a shared read guard and return the manifest immediately without any file IO.

**Phase 2: Segment Pruning**

The manifest exposes high-level statistics for each column involved in the predicate:

```text
manifest["schema_name"]["column_name"] → [Buffer { segment, count, min, max }]
```

The reader can use these statistics to eliminate segments where the query predicate is provably unsatisfiable.

```text
query.min > buffer.max  →  All values in segment are below the query range
query.max < buffer.min  →  All values in segment are above the query range
```

After pruning, the shared manifest read guard is released and the retained segments are passed to phase three.

**Phase 3: Lazy Async Batched Reads**

Candidate segments are packaged into a lazy async batched reader that chains across segments, presenting a flattened
stream of deserialized rows to the caller. Internally, the reader is batched to reduce syscall overhead; returning one
row each time `next` is called and only executing batched file IO when the internal buffer is exhausted.

**Concurrency Model**

Segments are immutable once written, meaning readers do not require coordination after extracting their list of
candidate segments in phase two. A concurrent writer appending a new segment must acquire an exclusive write guard to
update the manifest and file header. This temporarily blocks new readers from accessing the manifest, but does not
affect in-flight reads.

| Operation                              | Lock mode   | Duration                                   |
|----------------------------------------|-------------|--------------------------------------------|
| First manifest load                    | Write       | File header read + CBOR decode.            |
| Subsequent manifest access (all reads) | Shared read | Extracting the list of candidate segments. |
| Writer updating header + manifest      | Write       | Phases 2 and 4 of the write cycle only.    |
| Reading segment data from disk         | **None**    | Segments are immutable; no lock required.  |

This design ensures:

- **Multiple readers** can resolve the manifest and build candidate segment lists simultaneously.
- **A writer** updating the manifest does not block phase three readers.
- **Segment IO** is fully parallel; readers and writers never contend on per-segment data regions.

The read cycle is implemented by the `Query` builder.

---

### 7. Query Builder

The `Query` builder provides a composable interface for reading data from any `clem` file. Conditions are chained
method-by-method and evaluated lazily; no file IO occurs until `.read().await` is called.

```rust
let results = dataset
    .query("schema_name")
    .select(["latitude", "longitude", "temperature"])
    .range("temperature", 10.0..=20.0)
    .eq("active", true)
    .read()
    .await?;
```

##### 7.1 Projection

A query returns all columns defined by the schema unless otherwise specified. The `.select` method restricts the
returned columns to a named subset, reducing file IO to only the required buffers.

```rust
.select(["a", "b"]) // Return only columns "a" and "b"
```

Columns omitted from `select` are never read from disk. This is the primary mechanism to reduce file IO on wide schemas.
Omitting `select` is equivalent to selecting every column.

##### 7.2 Filters

Filters are used to exclude rows from the selected columns. All filters are **conjunctive** by default – each additional
filter appends an `AND` condition. Multiple filters on the same column are composed together. Segment pruning is applied
whenever the filter type maps to a statistic stored in the manifest. Row-level filtering is applied during Phase 3 for
predicates that cannot be fully resolved from manifest statistics alone.

**Range Filter**

Retain rows where the value in the specified column falls within a specified interval `[min, max]`. Directly exploits
the `min` and `max` buffer statistics for segment pruning.

```rust
.range("temperature", 10.0..=20.0) // 10.0 ≤ temperature ≤ 20.0 inclusive range
.range("altitude", 0.0..500.0)     // exclusive upper bound on additonal column
```

Open or half-open ranges are also supported:

```rust
.range("pressure", 101.3..) // pressure ≥ 101.3  (no upper bound)
.range("pressure", ..105.0) // pressure < 105.0  (no lower bound)
```

**Equality Filter**

Retain rows where the value in the specified column exactly equals a given value. Useful for boolean flags, integer
codes, and enum discriminants.

```rust
.eq("active", true)
.eq("sensor_id", 42u32)
```

Equality on an orderable type is equivalent to `.range(col, v..=v)` and benefits from segment pruning.

**Option Filter**

Retain or reject optional rows that contain `Some` or `None`. Exploits the null bitmap stored in each buffer. Returns
an error if the column type is not `Option`.

```rust
.is_some("calibration") // row must have a calibration value
.is_none("error_code")  // row must have no error code
```

**Set Membership Filter**

Retain or reject rows where the column value is a member of a finite set. Useful for allowlists, category codes, or
string tags. Orderable types benefit from segment pruning; skipped if `buffer.max < set.min || buffer.min > set.max`.

```rust
.one_of("sensor_id", [1u32, 4, 7, 12])
.none_of("status_code", [404u16, 500])
```

**Boolean Mask**

Retain rows by position using a pre-computed boolean column. Applies the named column as a bitmask; only rows where the
mask column is `true` are returned. The mask column is read in full alongside target columns (no segment pruning).

```rust
.mask("is_valid") // equivalent to .eq("is_valid", true) with cross-column semantics
```

**Limit and Offset**

Restrict the number of rows returned without a value-based predicate. Segment pruning uses `buffer.count` to skip
segments that fall entirely outside the requested window.

```rust
.limit(1000) // return at most 1000 rows
.offset(500) // skip the first 499 matching rows
.limit(1000).offset(500) // rows 500..1500
```

**Stride**

Sample every nth row from the result set. Useful for decimation and preview reads on dense time-series data.

```rust
.stride(10) // return every 10th row
```

##### 7.3 Execution and Output

`.read().await` executes the query and returns a lazy async batched reader. Each call to `.next().await`
yields one deserialized row; no row is deserialized before being requested. The reader chains across segments
transparently, meaning callers observe a flat sequence regardless of the underlying segment structure.

```rust
let mut cursor = dataset
    .query("schema_name")
    .select(["latitude", "longitude"])
    .range("altitude", 0.0..=1000.0)
    .read()
    .await?;
while let Some(row) = cursor.next().await { process(row?); }
```

A `.collect().await` convenience method collects the full result into an owned `Vec` for callers that require random
access over the result set.

```rust
let result: Vec<R> = dataset
    .query("schema_name")
    .eq("active", true)
    .collect()
    .await?;
```

##### 7.4 Filter Evaluation Order

Filters are evaluated in two stages to minimise IO:

| Stage | Scope   | Uses Manifest | Description                                             |
|-------|---------|---------------|---------------------------------------------------------|
| One   | Segment | Yes           | Discard entire segments manifest statistics.            |
| Two   | Row     | No            | Evaluate remaining predicates row-by-row during decode. |

Filters that can be fully satisfied by manifest statistics never cause unnecessary file IO. Filters that require
individual row values are combined and applied in a single pass across the candidate segments.

##### 7.5 Cross-Schema Queries

A single `clem` file may contain multiple schemas. A query builder can operate across schemas by joining two query legs
via a shared **key** column. Dictionaries store entries using ordinary schema and data segments, meaning the join
surface applies uniformly across named schemas and named dictionaries.

```rust
impl Query { pub fn join<S: Display>(&mut self, other: &mut Self, left: S, right: S) -> Self { ... } }
```

Users can `join` two existing `Query` instances into a single instance. Both legs are independent and therefore support
the full filter and projection vocabulary. The specified columns are matched by value and must share a compatible type.

**Join:** Retain only rows where the key is present in both legs.

```rust
let result = dataset
    .query("readings")
    .select(["time", "sensor", "value"])
    .range("value", 10.0..=20.0)
    .join(
        dataset.query("sensors").select(["id", "location"]),
        "sensor", // left key column
        "id", // right key column
    )
    .read()
    .await?
```

**Semi-join:** Retain rows from the left leg whose key appears in the right leg, but do not include any right-leg
columns in the output. Useful for existence filtering without column inflation.

```rust
.semi_join(
    dataset.query("active_sensors").eq("online", true),
    "sensor_id",
    "sensor_id",
)
```

**Anti-join:** The complement of semi-join. Retain rows from the left leg whose key does *not* appear in the right leg,
but do not include any right-leg columns in the output.

```rust
.anti_join(
    dataset.query("faulty_sensors"),
    "sensor_id",
    "sensor_id",
)
```

**Dictionary Joins**

Dictionaries expose the same `Query` interface as ordinary schemas, meaning dictionary joins are syntactically
identical. The dictionary **key** column acts as the natural join key. Manifest `min` and `max` statistics on the key
column are used for segment pruning on the dictionary side.

```rust
let result = dataset
    .query("readings")
    .select(["sensor_id", "value"])
    .join(
        dataset.query("sensor_dictionary").select(["id", "label"]),
        "sensor_id", // left key column
        "id", // dictionary key column
    )
    .read()
    .await?
```

**Chaining Joins**

The query builder is deliberately composable. Each `join` returns a new `Query` that can itself be further filtered or
joined.

```rust
let result = dataset
    .query("measurements")
    .range("time", t0..t1)
    .join(
        dataset.query("sensors").eq("online", true),
        "sensor_id",
        "sensor_id",
    )
    .join(
        dataset.query("locations_dictionary").select(["id", "region", "timezone"]),
        "sensor_id",
        "id",
    )
    .select(["time", "value", "region"])
    .read()
    .await?
```

Filters after a join apply to the combined output. Filters before a join apply only to the calling `Query` instance and
can therefore benefit from segment pruning prior to cross-schema file IO. The join strategy is selected automatically
based on `count` statistics:

| Size                     | Strategy                        |
|--------------------------|---------------------------------|
| Right leg fits in memory | Hash join                       |
| Both legs are large      | Disk-based external sort joins  |

##### 7.6 Filter Summary

| Method                  | Segment pruning | Row-level filter | Notes                                   |
|-------------------------|-----------------|------------------|-----------------------------------------|
| `.select([cols])`       | ✓ column IO     | —                | Skips unselected buffer reads entirely. |
| `.range(col, lo..hi)`   | ✓ `min` `max`   | ✓                | Core predicate; composable.             |
| `.eq(col, val)`         | ✓ `min` `max`   | ✓                | Equivalent to `range(col, val..=val)`.  |
| `.is_some(col)`         | —               | ✓                | Requires bitmap read.                   |
| `.is_none(col)`         | —               | ✓                | Requires bitmap read.                   |
| `.one_of(col, set)`     | ✓ `min` `max`   | ✓                | Prunes when set is disjoint from range. |
| `.none_of(col, set)`    | —               | ✓                |                                         |
| `.mask(col)`            | —               | ✓                | Cross-column boolean filter.            |
| `.limit(n)`             | ✓ `count`       | ✓                | Stops iteration once `n` rows yielded.  |
| `.offset(n)`            | ✓ `count`       | ✓                | Skips segments wholly before offset.    |
| `.stride(n)`            | —               | ✓                | Decimation; no segment-level skip.      |