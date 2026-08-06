/*
Project: msca
GitHub: https://github.com/MillieFD/msca

BSD 3-Clause License, Copyright (c) 2026, Amelia Fraser-Dale

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the conditions of the LICENSE are met.
*/

//! Public-facing user interface for [msca](crate) datasets.
//!
//! ---
//!
//! [`Dataset`] is the primary entry-point for working with a [msca](crate) file; providing a
//! high-level surface for registering [`Data`] types and [querying](query) stored data while
//! delegating low-level IO to an internal [`File`] handle.

use std::collections::hash_map::{Entry, VacantEntry};
use std::hash::Hash;
use std::path::Path;
use std::sync::Arc;

use funty::Unsigned;
use memmap2::Mmap;

use crate::io::{File, Register};
use crate::query::{self, Query};
use crate::read::Unfiltered;
use crate::schema::number;
use crate::segment::Segment;
use crate::{Accumulate, Accumulator, Data, Error, Schema, io, manifest};

/* ------------------------------------------------------------------------------ Public Exports */

/// A high-level handle to an open [`msca`](crate) dataset.
// TODO → Dataset is the main user interaction surface; doc must be comprehensive and clear.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug)]
pub struct Dataset {
    /// Underlying [`File`] handle backing this dataset.
    pub(crate) file: File,
    /// Read-only [memory map](Mmap) backed by the [msca](crate) file.
    ///
    /// ### ⚠️ Warning
    ///
    /// Undefined behaviour may occur if the mapped region is modified. The [`Mmap`] is therefore
    /// tightly scoped; mapping only the immutable segment region to reduce the risk of undefined
    /// behaviour. Refer to the [`File::mmap`] documentation for more details.
    pub mmap: Arc<Mmap>,
}

impl Dataset {
    /// Create a new empty [`Dataset`] at the specified [`path`](P).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the underlying system call fails. This can occur for a variety of
    /// reasons, including:
    ///
    /// - A file already exists at the specified [`path`](P)
    /// - The current process lacks read and write permissions
    /// - The platform does not support [memory mapping](memmap2)
    ///
    /// Returns [`Error::Zero`] if a `u64` overflow occurs while calculating `size` or `offset` for
    /// the relevant file regions.
    pub async fn new<P>(path: P) -> Result<Self, io::Error>
    where
        P: AsRef<Path>,
    {
        let file = File::create(path).await?;
        let mmap = unsafe { file.mmap()? }.into();
        Ok(Self { file, mmap })
    }

    /// Open an existing [`Dataset`] with read and write permissions at the specified [`path`](P).
    ///
    /// A [`Mmap`] is scoped to the immutable segment file region. Implementors must ensure that the
    /// provided [`path`](P) remains valid and accessible for the entire duration of the operation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the underlying system call fails. This can occur for a variety of
    /// reasons, including:
    ///
    /// - A file already exists at the specified [`path`](P)
    /// - The current process lacks read and write permissions
    /// - Unexpected `EOF` while parsing the [`Header`] or [`Manifest`]
    /// - The platform does not support [memory mapping](memmap2)
    ///
    /// Returns [`Error::Zero`] if a `u64` overflow occurs while calculating `size` or `offset` for
    /// the relevant file regions.
    pub async fn open<P>(path: P) -> Result<Self, io::Error>
    where
        P: AsRef<Path>,
    {
        let file = File::open(path).await?;
        let mmap = unsafe { file.mmap()? }.into();
        Ok(Self { file, mmap })
    }

    /// Register the [`Schema`] for [`I`] under `name` and return an empty [`Accumulator`].
    ///
    /// Unseen [`Schema`] are eagerly written to disk. Existing [`Schema`] are deduplicated without
    /// file [`IO`](io).
    ///
    /// ### Errors
    ///
    /// - [`io::Error::Schema`] wrapping [`Error::Collision`][1] if a schema is already registered
    ///   with the requested `name` but an incompatible column layout.
    /// - [`io::Error::Io`] if the underlying [write-cycle](io) fails.
    ///
    /// [1]: crate::schema::Error::Collision
    pub async fn schema<I>(&mut self, name: &str) -> Result<Accumulator<I>, io::Error>
    where
        I: Data,
    {
        let mut schema = Schema::new(name);
        let boxed = I::accumulator(&mut schema)?;
        Ok(Accumulator {
            data: boxed,
            name: name.to_string(),
            schema: schema.finish(self).await?,
        })
    }

    /// [`Write`](Segment::write) the accumulated data and return the number of written items.
    ///
    /// Empty accumulators are ignored. The [`Manifest`](manifest::Manifest) entry is reserved
    /// before file [`IO`](File::write); a rejected [`Segment`] leaves the file untouched.
    ///
    /// ### Errors
    ///
    /// - [`Error::Manifest`][1] wrapping [`Error::Collision`][2] if a name collision occurs.
    /// - [`Error::Io`][3] if the underlying [write-cycle](io) fails.
    /// - [`Error::Number`][4] if a `u64` overflow occurs while computing the on-disk [`Sector`][5].
    ///
    /// [1]: io::Error::Manifest
    /// [2]: manifest::Error::Collision
    /// [3]: io::Error::Io
    /// [4]: io::Error::Number
    /// [5]: io::Sector
    #[allow(
        private_bounds,
        reason = "segment and register are sealed implementation details"
    )]
    pub async fn write<I, S>(&mut self, acc: S) -> Result<u64, io::Error>
    where
        S: Accumulate<I> + Register + Segment,
        io::Error: From<S::Error>,
    {
        let count = match acc.is_empty() {
            true => return Ok(0),
            false => acc.count(),
        };
        self.file.write(acc).await?;
        // SAFETY: Undefined behaviour if mapped region is modified (refer to mmap documentation)
        self.mmap = unsafe { self.file.mmap()? }.into();
        Ok(count)
    }

    /// Returns a **stable on-disk index** per unique [`item`](I) – in request order – writing
    /// unseen items to the [`Dataset`].
    ///
    /// The provided [collection](S) is deduplicated in a single **O(N)** pass, with unseen items
    /// [pushed](Accumulate::push) to an in-memory [`Accumulator`]:
    ///
    /// - [`I`] matches an existing or accumulated item → Reuse existing [`index`](N).
    /// - [`I`] is genuinely unique → Assign the next available index.
    ///
    /// Any accumulated items are then written to the [`Dataset`] as a single [`Segment`][1]. An
    /// empty accumulator performs no [`IO`](io). Exclusive `&mut` access ensures indices are
    /// assigned atomically. Indices are guaranteed to remain stable due to the immutable nature of
    /// the on-disk segment region.
    ///
    /// ### Guidance
    ///
    /// Items are deduplicated according to their [`Hash`] implementation. Implementers are advised
    /// to manually implement the [`Hash`] trait for full control over deduplication behaviour. A
    /// [transparent][2] wrapper may be necessary to override a pre-existing hash implementation.
    ///
    /// This function is generic over [`N`]; implementers can specify any [`Unsigned`] integer type
    /// according to the expected number of unique items and the desired return type. An [`Error`]
    /// is raised if any index overflows [`N`].
    ///
    /// ### Errors
    ///
    /// - [`Error::Schema`] wrapping [`Error::Collision`][3] if a [`Schema`] is already registered
    ///   with the requested `name` but an incompatible column layout.
    /// - [`Error::Query`] if a failure occurs during [`Iterator`] construction.
    /// - [`Error::Number`] if an index overflows [`N`].
    /// - [`Error::Io`] if [deserialization][3] or the underlying [write-cycle](io) fails.
    ///
    /// [1]: crate::segment::Segment
    /// [2]: https://doc.rust-lang.org/nomicon/other-reprs.html#reprtransparent
    /// [3]: crate::schema::Error::Collision
    /// [3]: io::Deserialize

    pub async fn get_or_insert<N, I, S>(&mut self, name: &str, items: S) -> Result<Box<[N]>, Error>
    where
        N: Unsigned,
        S: IntoIterator<Item = I>,
        I: Data + Eq + Hash + 'static,
        I: for<'q> Unfiltered<'q>,
    {
        let mut acc = self.schema::<I>(name).await?;
        let query = self.query(name)?;
        let mut map = query.indexed::<I, N>()?;
        let count = query.count(); // initial number of items (includes duplicates)
        let mut next = N::try_from(count).ok();
        let items = items.into_iter();
        let mut out = Vec::with_capacity(items.size_hint().0);
        for item in items {
            out.push(match map.entry(item) {
                Entry::Occupied(entry) => *entry.get(),
                Entry::Vacant(entry) => Self::insert(entry, &mut next)?,
            });
        }
        let mut new: Box<[(I, N)]> = map.into_iter().filter(|e| e.1.as_u64() >= count).collect();
        new.sort_unstable_by_key(|e| e.1);
        new.into_iter().for_each(|e| acc.push(e.0));
        self.write(acc).await?;
        Ok(out.into_boxed_slice())
    }

    /// [`Insert`](VacantEntry::insert) the next available index into the provided [`VacantEntry`].
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Zero`][1] if the inserted index overflows [`N`].
    ///
    /// [1]: number::Error::Zero
    fn insert<I, N>(entry: VacantEntry<I, N>, next: &mut Option<N>) -> Result<N, number::Error>
    where
        N: Unsigned,
    {
        let index = next.ok_or(number::Error::Zero)?;
        *next = index.checked_add(N::ONE);
        entry.insert(index);
        Ok(index)
    }

    /// Initialise a new [`Query`] over the named [`Schema`](manifest::Schema).
    ///
    /// The query begins with **every** column and **every** buffer from the specified schema.
    /// Each [`Column`](manifest::Column) is filtered subtractively to reduce the result set. No
    /// file [`IO`](io) occurs until the query is executed via a terminal method such as
    /// [`Query::iter`].
    ///
    /// ### Errors
    ///
    /// Returns [`Error::Column`][2] if the requested `name` is not found in the [`Manifest`][3].
    ///
    /// [1]: manifest::Schema
    /// [2]: query::Error::Column
    /// [3]: manifest::Manifest
    pub fn query<'m>(&'m self, name: &str) -> Result<Query<'m>, query::Error> {
        let columns = self
            .file
            .manifest
            .schemas
            .get(name)
            .ok_or_else(|| query::Error::Column { name: name.into() })?
            .columns
            .iter()
            .map(query::Column::map) // Clone each entry
            .collect();
        Ok(Query {
            mmap: self.mmap.clone(), // Inexpensive Arc Clone
            columns,
        })
    }

    /// Read the on-disk [`Binary`] segment body for the requested [`name`](String).
    ///
    /// Returns an immutable zero-copy byte [slice][1] borrowed directly from the [`Mmap`].
    ///
    /// ### Errors
    ///
    /// - [`Error::Manifest`][2] wrapping [`NotFound`][3] if the manifest does not contain a binary
    ///   segment registered under the specified name.
    /// - [`Error::Truncated`][4] if the recorded sector extends beyond the mapped region.
    ///
    /// [1]: https://doc.rust-lang.org/std/primitive.slice.html
    /// [2]: io::Error::Manifest
    /// [3]: manifest::Error::NotFound
    /// [4]: io::Error::Truncated
    pub fn binary(&self, name: &str) -> Result<&[u8], io::Error> {
        self.file
            .manifest
            .bins
            .get(name)
            .ok_or_else(|| manifest::Error::NotFound { name: name.into() })?
            .slice(&self.mmap)
    }
}

/* --------------------------------------------------------------------------------------- Tests */

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use macro_rules_attribute::apply;
    use smol::lock::RwLock;
    use smol_macros::{Executor, test};

    use super::*;
    use crate::io::{File, SizedBuf};
    use crate::manifest::Buffer;
    use crate::manifest::Error::{Collision, NotFound};
    use crate::query::column::Column;
    use crate::schema::{self, Type, number};
    use crate::segment::Variant;
    use crate::{Bin, Columns, Describe, Serialize, accumulate};

    /* ---------------------------------------------------------------------------- Shared State */

    /// A sensor reading used to exercise the write path in white-box layout tests. The [`Data`]
    /// implementation below mirrors the code generated by `#[derive(Data)]`; msca-core cannot
    /// depend on the derive crate. Round-trip reads are covered by the integration tests in the
    /// project `tests` directory, which use the real procedural macros.
    #[derive(Debug, PartialEq)]
    struct Measurement {
        pressure: u32,
        temperature: f64,
    }

    /// Generated-style composite accumulator holding one concrete sub-accumulator per
    /// [`Measurement`] field, named exactly as `#[derive(Data)]` would name it.
    #[derive(Default)]
    struct MeasurementAccumulator {
        pressure: accumulate::Buffer<u32>,
        temperature: accumulate::Buffer<f64>,
    }

    impl Accumulate<Measurement> for MeasurementAccumulator {
        fn push(&mut self, item: Measurement) {
            self.pressure.push(item.pressure);
            self.temperature.push(item.temperature);
        }

        fn discard(&mut self) {
            self.pressure.discard();
            self.temperature.discard();
        }

        fn is_empty(&self) -> bool {
            self.pressure.is_empty()
        }

        fn count(&self) -> u64 {
            self.pressure.count()
        }
    }

    impl Describe<Measurement> for MeasurementAccumulator {
        fn buffers(
            &self,
            offset: u64,
            seg: u64,
            columns: &mut Columns,
        ) -> Result<u64, schema::Error> {
            // Columns are walked in name-sorted order: `pressure` precedes `temperature`.
            let offset = self.pressure.buffers(offset, seg, columns)?;
            self.temperature.buffers(offset, seg, columns)
        }
    }

    impl Serialize for MeasurementAccumulator {
        type Buffer = Vec<u8>;

        fn size(&self) -> Result<NonZeroU64, number::Error> {
            let pressure = SizedBuf::new(&self.pressure).size()?.get();
            let temperature = SizedBuf::new(&self.temperature).size()?.get();
            pressure.checked_add(temperature).and_then(NonZeroU64::new).ok_or(number::Error::Zero)
        }

        fn serialize_into<'a>(&self, buf: &'a mut [u8]) -> Result<&'a mut [u8], number::Error> {
            let buf = SizedBuf::new(&self.pressure).serialize_into(buf)?;
            SizedBuf::new(&self.temperature).serialize_into(buf)
        }

        fn serialize(&self) -> Result<Vec<u8>, number::Error> {
            let size = self.size()?.get().try_into().map_err(number::Error::from)?;
            let mut buf = vec![0u8; size];
            self.serialize_into(&mut buf)?;
            Ok(buf)
        }
    }

    impl Data for Measurement {
        type Acc = MeasurementAccumulator;

        fn accumulator(schema: &mut Schema) -> Result<Self::Acc, schema::Error> {
            Ok(MeasurementAccumulator {
                pressure: schema.column::<u32>("pressure")?,
                temperature: schema.column::<f64>("temperature")?,
            })
        }
    }

    /// A [`Measurement`] keyed by `pressure`; the temperature column tracks it so one `u32` fixture
    /// populates both columns.
    const fn reading(pressure: u32) -> Measurement {
        Measurement { pressure, temperature: pressure as f64 }
    }

    /// Unique scratch path for a layout test, cleared before use.
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("msca-{name}.msca"));
        std::fs::remove_file(&path).ok();
        path
    }

    /// Write each batch into the `readings` schema via the public [`Dataset`] API; one data segment
    /// per batch.
    async fn write(path: &Path, batches: &[&[u32]]) {
        let mut dataset = Dataset::new(path).await.expect("new failed");
        for values in batches {
            let mut acc = dataset.schema::<Measurement>("readings").await.expect("schema failed");
            values.iter().for_each(|&pressure| acc.push(reading(pressure)));
            dataset.write(acc).await.expect("write failed");
        }
    }

    /* ------------------------------------------------------------------------------ Unit Tests */

    /// Each committed buffer [`Sector`](crate::Sector) begins at a 64-bit alignment boundary.
    #[apply(test!)]
    async fn committed_buffers_align_to_boundary() {
        let path = scratch("align");
        write(&path, &[&[1, 2, 3], &[4, 5, 6, 7]]).await;
        let file = File::open(&path).await.expect("open failed");
        let offsets: Vec<u64> = file
            .manifest
            .schemas
            .values()
            .flat_map(|schema| schema.columns.values())
            .flat_map(|column| column.buffers.iter())
            .map(Buffer::offset)
            .collect();
        std::fs::remove_file(&path).ok();
        assert_eq!(offsets.len(), 4); // two segments × two columns
        offsets.iter().for_each(|offset| assert_eq!(offset % 8, 0));
    }

    /// A registered schema is persisted as an on-disk [`Schema`](crate::Schema) segment; the
    /// recorded [`Sector`](crate::Sector) points at the segment (variant `0x01`), never at the data
    /// segment that follows it.
    #[apply(test!)]
    async fn registered_schema_persists_as_a_segment() {
        let path = scratch("schema-persisted");
        write(&path, &[&[1, 2, 3]]).await;
        let file = File::open(&path).await.expect("open failed");
        let sector = file.manifest.schemas.get("readings").expect("schema missing").sector;
        let bytes = std::fs::read(&path).expect("read failed");
        std::fs::remove_file(&path).ok();
        assert_eq!(bytes[sector.offset as usize], Variant::Schema as u8);
    }

    /// Each data segment header references the on-disk schema segment by offset; the pointer
    /// resolves to a real [`Schema`](crate::Schema) segment rather than dangling into unrelated
    /// bytes.
    #[apply(test!)]
    async fn data_segment_points_back_to_its_schema() {
        // 1. Commit one segment and recover the schema sector it should point at.
        let path = scratch("schema-pointer");
        write(&path, &[&[1, 2, 3]]).await;
        let file = File::open(&path).await.expect("open failed");
        let sector = file.manifest.schemas.get("readings").expect("schema missing").sector;
        let bytes = std::fs::read(&path).expect("read failed");
        std::fs::remove_file(&path).ok();

        // 2. The data segment begins immediately after the schema segment
        let data = sector.next().expect("sector overflow").get() as usize;
        let ptr = u64::from_le_bytes(bytes[data + 9..data + 17].try_into().expect("8 bytes"));

        // 3. The recorded pointer resolves to the schema segment.
        assert_eq!(bytes[data], Variant::Data as u8);
        assert_eq!(ptr, sector.offset);
    }

    /// Registering an incompatible schema under an existing name is rejected, and the original
    /// registration is left intact — never overwritten.
    #[apply(test!)]
    async fn incompatible_schema_reregistration_is_rejected() {
        // 1. Register `pressure` as a `u32` column.
        let path = scratch("schema-collision");
        let mut dataset = Dataset::new(&path).await.expect("new failed");
        let mut first = Schema::new("readings");
        first.column::<u32>("pressure").expect("column failed");
        first.finish(&mut dataset).await.expect("first finish failed");

        // 2. Re-register the same name with an incompatible column type.
        let mut second = Schema::new("readings");
        second.column::<u64>("pressure").expect("column failed");
        second.finish(&mut dataset).await.expect_err("Incompatible schema accepted");

        // 3. The original registration survives untouched.
        let column = dataset.file.manifest.schemas.get("readings").expect("schema missing");
        let ty = column.columns.get("pressure").expect("column missing").ty.clone();
        std::fs::remove_file(&path).ok();
        assert_eq!(ty, Type::U32);
    }

    /// A single-byte corruption inside the manifest region is detected when the file is reopened;
    /// the file header points at a manifest segment whose trailing checksum no longer verifies.
    #[apply(test!)]
    async fn manifest_corruption_is_detected_on_open() {
        let path = scratch("corrupt-manifest");
        write(&path, &[&[1, 2, 3]]).await;
        let mut bytes = std::fs::read(&path).expect("read failed");
        let last = bytes.len() - 1;
        bytes[last] ^= u8::MAX; // corrupt the manifest segment checksum trailer
        std::fs::write(&path, &bytes).expect("write failed");
        let err = File::open(&path).await.expect_err("corruption undetected");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, io::Error::Checksum));
    }

    /// A [`Bin`] payload written through the generalised [`Dataset::write`] round-trips via
    /// [`Dataset::binary`] – immediately and after reopening – and the returned slice begins at a
    /// 64-bit boundary.
    #[apply(test!)]
    async fn binary_payload_round_trips_through_a_dataset() {
        // 1. Write one named binary payload.
        let path = scratch("bin-round-trip");
        let mut dataset = Dataset::new(&path).await.expect("New failed");
        let mut bin = Bin::new("cal");
        bin.push(b"format-agnostic".as_slice());
        let count = dataset.write(bin).await.expect("Write failed");

        // 2. Read it back from the live dataset.
        let data = dataset.binary("cal").expect("Read failed");
        let aligned = (data.as_ptr() as usize).is_multiple_of(8);
        assert_eq!(count, 15);
        assert_eq!(data, b"format-agnostic");
        assert!(aligned); // absolute 64-bit alignment

        // 3. Reopen from disk and confirm the payload survived.
        drop(dataset);
        let dataset = Dataset::open(&path).await.expect("Open failed");
        let reread = dataset.binary("cal").expect("Reread failed");
        std::fs::remove_file(&path).ok();
        assert_eq!(reread, b"format-agnostic");
    }

    /// Rewriting an existing name is rejected before file IO and the original payload survives
    /// untouched; an unknown name reports [`NotFound`](manifest::Error::NotFound).
    #[apply(test!)]
    async fn binary_payload_is_immutable_once_written() {
        // 1. Write the original payload.
        let path = scratch("bin-immutable");
        let mut dataset = Dataset::new(&path).await.expect("New failed");
        let mut bin = Bin::new("cal");
        bin.push([1u8, 2].as_slice());
        dataset.write(bin).await.expect("Write failed");

        // 2. A second write under the same name is rejected.
        let mut twin = Bin::new("cal");
        twin.push([3u8, 4].as_slice());
        let clash = dataset.write(twin).await.expect_err("Duplicate accepted");

        // 3. The original survives, and an unknown name reports NotFound.
        let kept = dataset.binary("cal").expect("Read failed");
        let absent = dataset.binary("nope").expect_err("Phantom bin found");
        std::fs::remove_file(&path).ok();
        assert!(matches!(clash, io::Error::Manifest(Collision { .. })));
        assert_eq!(kept, &[1, 2]); // the original payload is untouched
        assert!(matches!(absent, io::Error::Manifest(NotFound { .. })));
    }

    /// An empty [`Bin`] accumulator performs no file [`IO`](io) and registers nothing.
    #[apply(test!)]
    async fn empty_binary_accumulator_is_ignored() {
        let path = scratch("bin-empty");
        let mut dataset = Dataset::new(&path).await.expect("New failed");
        let count = dataset.write(Bin::new("void")).await.expect("Write failed");
        let registered = dataset.file.manifest.bins.is_empty();
        std::fs::remove_file(&path).ok();
        assert_eq!(count, 0);
        assert!(registered); // nothing was recorded
    }

    /// Distinct schemas committed to one shared [`Dataset`] by **interleaved** concurrent writers
    /// each read back their own items intact, whatever order the segments land in.
    ///
    /// The writers contend on a [`RwLock`] guarding the single dataset, so the executor interleaves
    /// them at every `await`. Because every data segment is self-describing – it points back at its
    /// own schema – the committed order never changes what each schema reads back. This lets
    /// independent producers share one dataset without coordinating their writes.
    #[apply(test!)]
    async fn interleaved_writers_read_back_independent_of_order(ex: &Executor<'_>) {
        // 1. One dataset shared behind an async lock; every writer contends for it.
        let path = scratch("interleave");
        let dataset = Arc::new(RwLock::new(Dataset::new(&path).await.expect("new failed")));
        let batches: [(&str, Vec<u32>); 4] = [
            ("north", vec![1, 2, 3]),
            ("south", vec![10, 20]),
            ("east", vec![100]),
            ("west", vec![7, 7, 7, 7]),
        ];

        // 2. Fork: each writer commits its own schema, interleaving at every await.
        let writers: Vec<_> = batches
            .iter()
            .cloned()
            .map(|(name, values)| {
                let dataset = dataset.clone();
                ex.spawn(async move {
                    let mut guard = dataset.write().await;
                    let mut acc = guard.schema::<Measurement>(name).await.expect("schema failed");
                    values.into_iter().for_each(|v| acc.push(reading(v)));
                    guard.write(acc).await.expect("write failed");
                })
            })
            .collect();

        // 3. Join: drain every spawned writer before reading anything back.
        for task in writers {
            task.await;
        }

        // 4. Read each schema back through a shared read lock.
        let guard = dataset.read().await;
        let readback: Vec<(&str, Vec<u32>)> = batches
            .iter()
            .map(|(name, _)| {
                let query = guard.query(name).expect("query failed");
                let column = query.column::<u32>("pressure").expect("column failed");
                let items = column.read().expect("read failed");
                (
                    *name,
                    items.collect::<Result<Vec<u32>, _>>().expect("collect failed"),
                )
            })
            .collect();
        drop(guard);
        std::fs::remove_file(&path).ok();

        // 5. Every schema holds exactly the items its own writer pushed.
        for (name, values) in batches {
            let found = &readback.iter().find(|leg| leg.0 == name).expect("schema missing").1;
            assert_eq!(found, &values, "{name} read back the wrong items");
        }
    }
}
