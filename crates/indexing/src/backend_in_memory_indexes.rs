use std::{
    cmp,
    collections::{
        BTreeMap,
        BTreeSet,
    },
    sync::Arc,
};

use anyhow::Context;
use async_trait::async_trait;
use common::{
    bootstrap_model::index::{
        database_index::DatabaseIndexState,
        IndexConfig,
    },
    document::{
        PackedDocument,
        ResolvedDocument,
    },
    index::{
        IndexKey,
        IndexKeyBytes,
    },
    instrument,
    interval::{
        Interval,
        IntervalSet,
    },
    persistence::PersistenceSnapshot,
    query::Order,
    static_span,
    types::{
        DatabaseIndexUpdate,
        DatabaseIndexValue,
        GenericIndexName,
        IndexId,
        IndexName,
        RepeatableTimestamp,
        TabletIndexName,
        Timestamp,
    },
    utils::ReadOnly,
    value::Size,
};
use errors::ErrorMetadata;
use futures::TryStreamExt;
use imbl::OrdMap;
use itertools::Itertools;
use value::{
    ResolvedDocumentId,
    TableId,
    TableMapping,
    TableName,
};

use crate::{
    index_registry::IndexRegistry,
    metrics::log_transaction_cache_query,
};

#[async_trait]
pub trait InMemoryIndexes: Send + Sync {
    /// Returns the index range if it is found in the cache (backend) or loaded
    /// into the cache (function runner). If the index is not supposed to be in
    /// memory, returns None so it is safe to call on any index.
    async fn range(
        &self,
        index_id: IndexId,
        interval: &Interval,
        order: Order,
        table_id: TableId,
        table_name: TableName,
    ) -> anyhow::Result<Option<Vec<(IndexKeyBytes, Timestamp, ResolvedDocument)>>>;
}

/// [`BackendInMemoryIndexes`] maintains in-memory database indexes. With the
/// exception of the table scan index, newly created indexes are not initially
/// loaded in memory. A post-commit, asynchronous backfill job is responsible
/// for filling the index.
#[derive(Clone)]
pub struct BackendInMemoryIndexes {
    /// Fully loaded in-memory indexes. If not present, the index is not loaded.
    in_memory_indexes: OrdMap<IndexId, DatabaseIndexMap>,
}

#[async_trait]
impl InMemoryIndexes for BackendInMemoryIndexes {
    async fn range(
        &self,
        index_id: IndexId,
        interval: &Interval,
        order: Order,
        _table_id: TableId,
        _table_name: TableName,
    ) -> anyhow::Result<Option<Vec<(IndexKeyBytes, Timestamp, ResolvedDocument)>>> {
        Ok(self
            .in_memory_indexes
            .get(&index_id)
            .map(|index_map| order.apply(index_map.range(interval)).collect()))
    }
}

impl BackendInMemoryIndexes {
    pub fn bootstrap(
        index_registry: &IndexRegistry,
        index_documents: BTreeMap<ResolvedDocumentId, (Timestamp, ResolvedDocument)>,
        ts: Timestamp,
    ) -> anyhow::Result<Self> {
        // Load the indexes by_id index
        let meta_index = index_registry
            .get_enabled(&TabletIndexName::by_id(
                index_registry.index_table().table_id,
            ))
            .context("Missing meta index")?;
        let mut meta_index_map = DatabaseIndexMap::new_at(ts);
        for (ts, index_doc) in index_documents.into_values() {
            let index_key = IndexKey::new(vec![], (*index_doc.id()).into());
            meta_index_map.insert(index_key.into_bytes(), ts, index_doc);
        }

        let mut in_memory_indexes = OrdMap::new();
        in_memory_indexes.insert(meta_index.id(), meta_index_map);

        Ok(Self { in_memory_indexes })
    }

    pub async fn load_enabled_for_tables(
        &mut self,
        index_registry: &IndexRegistry,
        table_mapping: &TableMapping,
        snapshot: &PersistenceSnapshot,
        tables: &BTreeSet<TableName>,
    ) -> anyhow::Result<()> {
        for index_metadata in index_registry.all_enabled_indexes() {
            let table_name = table_mapping.tablet_name(*index_metadata.name.table())?;
            if tables.contains(&table_name) {
                match &index_metadata.config {
                    IndexConfig::Database { on_disk_state, .. } => {
                        anyhow::ensure!(
                            *on_disk_state == DatabaseIndexState::Enabled,
                            "Index should have been enabled: {:?}, state: {on_disk_state:?}",
                            index_metadata.name
                        )
                    },
                    IndexConfig::Search { .. } | IndexConfig::Vector { .. } => {
                        // We do not load search or vector indexes into memory.
                        continue;
                    },
                }
                tracing::info!(
                    "Loading {table_name}.{} ...",
                    index_metadata.name.descriptor()
                );
                let (num_keys, total_bytes) = self
                    .load_enabled(index_registry, &index_metadata.name, snapshot)
                    .await?;
                tracing::info!("Loaded {num_keys} keys, {total_bytes} bytes.");
            }
        }
        Ok(())
    }

    pub async fn load_enabled(
        &mut self,
        index_registry: &IndexRegistry,
        index_name: &TabletIndexName,
        snapshot: &PersistenceSnapshot,
    ) -> anyhow::Result<(usize, usize)> {
        let index = index_registry
            .get_enabled(index_name)
            .ok_or_else(|| anyhow::anyhow!("Attempting to load missing index {}", index_name))?;
        if self.in_memory_indexes.contains_key(&index.id()) {
            // Already loaded in memory.
            return Ok((0, 0));
        }
        if let IndexConfig::Database { on_disk_state, .. } = &index.metadata.config {
            anyhow::ensure!(
                *on_disk_state == DatabaseIndexState::Enabled,
                "Attempting to load index {} that is not backfilled yet {:?}",
                index.name(),
                index.metadata,
            );
        } else {
            anyhow::bail!(
                "Attempted to load index {} that isn't a database index {:?}",
                index.name(),
                index.metadata
            )
        }

        let entries: Vec<_> = snapshot
            .index_scan(
                index.id(),
                *index_name.table(),
                &Interval::all(),
                Order::Asc,
                usize::MAX,
            )
            .try_collect()
            .await?;
        let mut num_keys: usize = 0;
        let mut total_size: usize = 0;
        let mut index_map = DatabaseIndexMap::new_at(*snapshot.timestamp());
        for (key, ts, doc) in entries.into_iter() {
            num_keys += 1;
            total_size += doc.value().size();
            index_map.insert(key, ts, doc);
        }

        self.in_memory_indexes.insert(index.id(), index_map);
        Ok((num_keys, total_size))
    }

    pub fn update(
        &mut self,
        // NB: We assume that `index_registry` has already received this update.
        index_registry: &IndexRegistry,
        ts: Timestamp,
        deletion: Option<ResolvedDocument>,
        insertion: Option<ResolvedDocument>,
    ) -> Vec<DatabaseIndexUpdate> {
        if let (Some(old_document), None) = (&deletion, &insertion) {
            if *old_document.table() == index_registry.index_table() {
                // Drop the index from memory.
                self.in_memory_indexes
                    .remove(&old_document.id().internal_id());
            }
        }

        // Build up the list of updates to apply to all database indexes.
        let updates = index_registry.index_updates(deletion.as_ref(), insertion.as_ref());

        // Apply the updates to the subset of database indexes in memory.
        for update in &updates {
            match self.in_memory_indexes.get_mut(&update.index_id) {
                Some(key_set) => match &update.value {
                    DatabaseIndexValue::Deleted => {
                        key_set.remove(&update.key, ts);
                    },
                    DatabaseIndexValue::NonClustered(ref doc_id) => {
                        // All in-memory indexes are clustered. Get the document
                        // from the update itself.
                        match insertion {
                            Some(ref doc) => {
                                assert_eq!(doc_id, doc.id());
                                key_set.insert(update.key.clone().into_bytes(), ts, doc.clone());
                            },
                            None => panic!("Unexpected index update: {:?}", update.value),
                        }
                    },
                },
                None => {},
            };
        }

        updates
    }

    pub fn in_memory_indexes_last_modified(&self) -> BTreeMap<IndexId, Timestamp> {
        self.in_memory_indexes
            .iter()
            .map(|(index_id, index_map)| (*index_id, index_map.last_modified))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn in_memory_indexes(&self) -> OrdMap<IndexId, DatabaseIndexMap> {
        self.in_memory_indexes.clone()
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseIndexMap {
    // We use OrdMap to provide efficient copy-on-write.
    // Note that all in-memory indexes are clustered.
    inner: OrdMap<Vec<u8>, (Timestamp, PackedDocument)>,
    /// The timestamp of the last update to the index.
    last_modified: Timestamp,
}

impl DatabaseIndexMap {
    /// Construct an empty set.
    fn new_at(ts: Timestamp) -> Self {
        Self {
            inner: OrdMap::new(),
            last_modified: ts,
        }
    }

    /// The number of keys in the index.
    #[cfg(any(test, feature = "testing"))]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns an iterator over the index that are within `range`, in order.
    fn range(
        &self,
        interval: &Interval,
    ) -> impl DoubleEndedIterator<Item = (IndexKeyBytes, Timestamp, ResolvedDocument)> + '_ {
        let _s = static_span!();
        self.inner
            .range(interval)
            .map(|(k, (ts, v))| (IndexKeyBytes(k.clone()), *ts, v.unpack()))
    }

    fn insert(&mut self, k: IndexKeyBytes, ts: Timestamp, v: ResolvedDocument) {
        self.inner.insert(k.0, (ts, PackedDocument::pack(v)));
        self.last_modified = cmp::max(self.last_modified, ts);
    }

    fn remove(&mut self, k: &IndexKey, ts: Timestamp) {
        let k = k.clone().into_bytes().0;
        self.inner.remove(&k);
        self.last_modified = cmp::max(self.last_modified, ts);
    }
}

/// Represents the state of the index at a certain snapshot of persistence.
#[derive(Clone)]
pub struct DatabaseIndexSnapshot {
    index_registry: ReadOnly<IndexRegistry>,
    in_memory_indexes: Arc<dyn InMemoryIndexes>,
    table_mapping: ReadOnly<TableMapping>,

    persistence: PersistenceSnapshot,

    // Cache results reads from the snapshot. The snapshot is immutable and thus
    // we don't have to do any invalidation.
    cache: DatabaseIndexSnapshotCache,
}

impl DatabaseIndexSnapshot {
    pub fn new(
        index_registry: IndexRegistry,
        in_memory_indexes: Arc<dyn InMemoryIndexes>,
        table_mapping: TableMapping,
        persistence_snapshot: PersistenceSnapshot,
    ) -> Self {
        Self {
            index_registry: ReadOnly::new(index_registry),
            in_memory_indexes,
            table_mapping: ReadOnly::new(table_mapping),
            persistence: persistence_snapshot,
            cache: DatabaseIndexSnapshotCache::new(),
        }
    }

    /// Query the given index at the snapshot.
    pub async fn range(
        &mut self,
        index_name: TabletIndexName,
        printable_index_name: &IndexName,
        interval: Interval,
        order: Order,
        size_hint: usize,
    ) -> anyhow::Result<(Vec<(IndexKeyBytes, Timestamp, ResolvedDocument)>, Interval)> {
        let index = match self
            .index_registry
            .require_enabled(&index_name, printable_index_name)
        {
            Ok(index) => index,
            // Allow default system defined indexes on all tables other than the _index table.
            Err(_)
                if index_name.table() != &self.index_registry.index_table().table_id
                    && index_name.is_by_id_or_creation_time() =>
            {
                return Ok((vec![], Interval::empty()));
            },
            Err(e) => anyhow::bail!(e),
        };

        // Check that the index is indeed a database index.
        let IndexConfig::Database { on_disk_state, .. } = &index.metadata.config else {
            let err = index_not_a_database_index_error(
                &index_name.map_table(&self.table_mapping.tablet_to_name())?,
            );
            anyhow::bail!(err);
        };
        anyhow::ensure!(
            *on_disk_state == DatabaseIndexState::Enabled,
            "Index returned from `require_enabled` but not enabled?"
        );

        // Now that we know it's a database index, serve it from the pinned
        // in-memory index if it's there.
        if let Some(range) = self
            .in_memory_indexes
            .range(
                index.id(),
                &interval,
                order,
                *index_name.table(),
                printable_index_name.table().clone(),
            )
            .await?
        {
            return Ok((range, Interval::empty()));
        }

        // Next, try the transaction cache.
        let cache_results = self.cache.get(index.id(), &interval, order);
        let mut results = vec![];
        for cache_result in cache_results {
            match cache_result {
                DatabaseIndexSnapshotCacheResult::Document(index_key, ts, document) => {
                    // Serve from cache.
                    log_transaction_cache_query(true);
                    results.push((index_key, ts, document));
                },
                DatabaseIndexSnapshotCacheResult::CacheMiss(interval) => {
                    log_transaction_cache_query(false);
                    // Query persistence.
                    let mut stream = self.persistence.index_scan(
                        index.id(),
                        *index_name.table(),
                        &interval,
                        order,
                        size_hint,
                    );
                    while let Some((key, ts, doc)) =
                        instrument!(b"Persistence::try_next", stream.try_next()).await?
                    {
                        // Populate all index point lookups that can result in the given
                        // document.
                        for (some_index, index_key) in self.index_registry.index_keys(&doc) {
                            self.cache.populate(
                                some_index.id(),
                                index_key.into_bytes(),
                                ts,
                                doc.clone(),
                            );
                        }
                        results.push((key.clone(), ts, doc));
                        if results.len() >= size_hint {
                            break;
                        }
                    }
                },
            }
            if results.len() >= size_hint {
                let last_key = results
                    .last()
                    .expect("should be at least one result")
                    .0
                    .clone();
                // Record the partial interval as cached.
                let (interval_read, interval_remaining) = interval.split(last_key, order);
                self.cache
                    .record_interval_populated(index.id(), interval_read);
                return Ok((results, interval_remaining));
            }
        }
        // After all documents in an index interval have been
        // added to the cache with `populate_cache`, record the entire interval as
        // being populated.
        self.cache.record_interval_populated(index.id(), interval);
        Ok((results, Interval::empty()))
    }

    /// Lookup the latest value of a document by id. Returns the document and
    /// the timestamp it was written at.
    pub async fn lookup_document_with_ts(
        &mut self,
        id: ResolvedDocumentId,
    ) -> anyhow::Result<Option<(ResolvedDocument, Timestamp)>> {
        let index_name = GenericIndexName::by_id(id.table().table_id);
        let printable_index_name = index_name
            .clone()
            .map_table(&self.table_mapping.tablet_to_name())?;
        let index_key = IndexKey::new(vec![], id.into());
        let range = Interval::prefix(index_key.into_bytes().into());

        // We call next() twice due to the verification below.
        let size_hint = 2;
        let (stream, remaining_interval) = self
            .range(
                index_name,
                &printable_index_name,
                range,
                Order::Asc,
                size_hint,
            )
            .await?;
        let mut stream = stream.into_iter();
        match stream.next() {
            Some((key, ts, doc)) => {
                assert!(
                    stream.next().is_none(),
                    "Got multiple values for key {:?}",
                    key
                );
                assert!(remaining_interval.is_empty());
                Ok(Some((doc, ts)))
            },
            None => Ok(None),
        }
    }

    pub fn timestamp(&self) -> RepeatableTimestamp {
        self.persistence.timestamp()
    }
}

const MAX_TRANSACTION_CACHE_SIZE: usize = 10 * (1 << 20); // 10 MiB

#[derive(Clone)]
struct DatabaseIndexSnapshotCache {
    /// Cache structure:
    /// Each document is stored, keyed by its index key for each index.
    /// Then for each index we have a set of intervals that are fully populated.
    /// The documents are populated first, then the intervals that contain them.
    ///
    /// For example, suppose a query does
    /// db.query('users').withIndex('by_age', q=>q.gt('age', 18)).collect()
    ///
    /// This will first populate `documents` with
    /// by_age -> <age:30, id:alice> -> (ts:100, { <alice document> })
    /// by_id -> <id:alice> -> (ts:100, { <alice document> })
    /// And it will populate the intervals:
    /// by_age -> <age:30, id:alice>
    /// by_id -> <id:alice>
    /// And it will do this for each document found.
    /// After the query is complete, we insert the final interval, which merges
    /// with the existing intervals:
    /// by_age -> (<age:18>, Unbounded)
    ///
    /// After the cache has been fully populated, `db.get`s which do point
    /// queries against by_id will be cached, and any indexed query against
    /// by_age that is a subset of (<age:18>, Unbounded) will be cached.
    documents: OrdMap<IndexId, BTreeMap<IndexKeyBytes, (Timestamp, ResolvedDocument)>>,
    intervals: OrdMap<IndexId, IntervalSet>,
    cache_size: usize,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum DatabaseIndexSnapshotCacheResult {
    Document(IndexKeyBytes, Timestamp, ResolvedDocument),
    CacheMiss(Interval),
}

/// How many persistence index scans to do for a single interval.
/// If some results are cached, we can do multiple index scans to avoid
/// re-fetching the cached results. But we don't want to perform too many index
/// scans because there is fixed overhead for each one.
const MAX_CACHED_RANGES_PER_INTERVAL: usize = 3;

impl DatabaseIndexSnapshotCache {
    fn new() -> Self {
        Self {
            documents: OrdMap::new(),
            intervals: OrdMap::new(),
            cache_size: 0,
        }
    }

    fn populate(
        &mut self,
        index_id: IndexId,
        index_key_bytes: IndexKeyBytes,
        ts: Timestamp,
        doc: ResolvedDocument,
    ) {
        let _s = static_span!();
        // Allow cache to exceed max size by one document, so we can detect that
        // the cache has maxed out.
        if self.cache_size <= MAX_TRANSACTION_CACHE_SIZE {
            let result_size: usize = doc.value().size();
            let interval = Interval::prefix(index_key_bytes.clone().into());
            self.documents
                .entry(index_id)
                .or_default()
                .insert(index_key_bytes, (ts, doc));
            self.intervals.entry(index_id).or_default().add(interval);
            self.cache_size += result_size;
        }
    }

    fn record_interval_populated(&mut self, index_id: IndexId, interval: Interval) {
        if self.cache_size <= MAX_TRANSACTION_CACHE_SIZE {
            self.intervals.entry(index_id).or_default().add(interval);
        }
    }

    fn get(
        &self,
        index_id: IndexId,
        interval: &Interval,
        order: Order,
    ) -> Vec<DatabaseIndexSnapshotCacheResult> {
        let components = match self.intervals.get(&index_id) {
            None => {
                return vec![DatabaseIndexSnapshotCacheResult::CacheMiss(
                    interval.clone(),
                )]
            },
            Some(interval_set) => interval_set.split_interval_components(interval),
        };
        let mut results = vec![];
        let mut cache_hit_count = 0;
        for (in_set, component_interval) in components {
            // There are better ways to pick which cached intervals to use
            // (use the biggest ones, allow an extra if it's at the end),
            // but those are more complicated to implement so we can improve when a
            // use-case requires it. For now we pick the first cached ranges
            // until we hit `MAX_CACHED_RANGES_PER_INTERVAL`.
            if cache_hit_count >= MAX_CACHED_RANGES_PER_INTERVAL {
                results.push(DatabaseIndexSnapshotCacheResult::CacheMiss(Interval {
                    start: component_interval.start,
                    end: interval.end.clone(),
                }));
                break;
            }
            if in_set {
                cache_hit_count += 1;
                match self.documents.get(&index_id) {
                    None => {},
                    Some(range) => {
                        results.extend(range.range(&component_interval).map(
                            |(index_key, (ts, doc))| {
                                DatabaseIndexSnapshotCacheResult::Document(
                                    index_key.clone(),
                                    *ts,
                                    doc.clone(),
                                )
                            },
                        ));
                    },
                }
            } else {
                results.push(DatabaseIndexSnapshotCacheResult::CacheMiss(
                    component_interval,
                ));
            }
        }
        order.apply(results.into_iter()).collect_vec()
    }
}

pub fn index_not_a_database_index_error(name: &IndexName) -> ErrorMetadata {
    ErrorMetadata::bad_request(
        "IndexNotADatabaseIndex",
        format!("Index {name} is not a database index"),
    )
}

#[cfg(test)]
mod cache_tests {
    use common::{
        bootstrap_model::index::database_index::IndexedFields,
        document::{
            CreationTime,
            ResolvedDocument,
        },
        interval::{
            BinaryKey,
            End,
            Interval,
            Start,
        },
        query::Order,
        testing::TestIdGenerator,
        types::{
            PersistenceVersion,
            Timestamp,
        },
    };
    use value::{
        assert_obj,
        val,
        values_to_bytes,
    };

    use super::DatabaseIndexSnapshotCache;
    use crate::backend_in_memory_indexes::DatabaseIndexSnapshotCacheResult;

    #[test]
    fn cache_point_lookup() -> anyhow::Result<()> {
        let mut cache = DatabaseIndexSnapshotCache::new();
        let mut id_generator = TestIdGenerator::new();
        let index_id = id_generator.generate_internal();
        let id = id_generator.generate(&"users".parse()?);
        let doc = ResolvedDocument::new(id, CreationTime::ONE, assert_obj!())?;
        let index_key_bytes = doc
            .index_key(&IndexedFields::by_id(), PersistenceVersion::default())
            .into_bytes();
        let ts = Timestamp::must(100);
        cache.populate(index_id, index_key_bytes.clone(), ts, doc.clone());

        let cached_result = cache.get(
            index_id,
            &Interval::prefix(values_to_bytes(&[Some(id.into())]).into()),
            Order::Asc,
        );
        assert_eq!(
            cached_result,
            vec![DatabaseIndexSnapshotCacheResult::Document(
                index_key_bytes,
                ts,
                doc
            )]
        );
        Ok(())
    }

    #[test]
    fn cache_full_interval() -> anyhow::Result<()> {
        let mut cache = DatabaseIndexSnapshotCache::new();
        let mut id_generator = TestIdGenerator::new();
        let index_id = id_generator.generate_internal();
        let id1 = id_generator.generate(&"users".parse()?);
        let doc1 = ResolvedDocument::new(id1, CreationTime::ONE, assert_obj!("age" => 30.0))?;
        let fields = vec!["age".parse()?];
        let index_key_bytes1 = doc1
            .index_key(&fields, PersistenceVersion::default())
            .into_bytes();
        let ts1 = Timestamp::must(100);
        cache.populate(index_id, index_key_bytes1.clone(), ts1, doc1.clone());

        let id2 = id_generator.generate(&"users".parse()?);
        let doc2 = ResolvedDocument::new(id2, CreationTime::ONE, assert_obj!("age" => 40.0))?;
        let index_key_bytes2 = doc2
            .index_key(&fields, PersistenceVersion::default())
            .into_bytes();
        let ts2 = Timestamp::must(150);
        cache.populate(index_id, index_key_bytes2.clone(), ts2, doc2.clone());

        let interval_gt_18 = Interval {
            start: Start::Included(values_to_bytes(&[Some(val!(18.0))]).into()),
            end: End::Unbounded,
        };

        let d = DatabaseIndexSnapshotCacheResult::Document;
        let cache_miss = DatabaseIndexSnapshotCacheResult::CacheMiss;
        // All documents populated but we don't know what the queried interval is.
        assert_eq!(
            cache.get(index_id, &interval_gt_18, Order::Asc),
            vec![
                cache_miss(Interval {
                    start: interval_gt_18.start.clone(),
                    end: End::Excluded(index_key_bytes1.clone().into()),
                }),
                d(index_key_bytes1.clone(), ts1, doc1.clone()),
                cache_miss(Interval {
                    start: Start::Included(
                        BinaryKey::from(index_key_bytes1.clone())
                            .increment()
                            .unwrap()
                    ),
                    end: End::Excluded(index_key_bytes2.clone().into()),
                }),
                d(index_key_bytes2.clone(), ts2, doc2.clone()),
                cache_miss(Interval {
                    start: Start::Included(
                        BinaryKey::from(index_key_bytes2.clone())
                            .increment()
                            .unwrap()
                    ),
                    end: End::Unbounded,
                }),
            ]
        );
        // Impossible interval (e.g. age > 18 && age < 16) is always cached.
        let interval_impossible = Interval {
            start: Start::Included(BinaryKey::min()),
            end: End::Excluded(BinaryKey::min()),
        };
        assert_eq!(
            cache.get(index_id, &interval_impossible, Order::Asc),
            vec![]
        );

        cache.record_interval_populated(index_id, interval_gt_18.clone());

        assert_eq!(
            cache.get(index_id, &interval_gt_18, Order::Asc),
            vec![
                d(index_key_bytes1.clone(), ts1, doc1.clone()),
                d(index_key_bytes2.clone(), ts2, doc2.clone()),
            ]
        );
        // Reverse order also cached.
        assert_eq!(
            cache.get(index_id, &interval_gt_18, Order::Desc),
            vec![
                d(index_key_bytes2.clone(), ts2, doc2.clone()),
                d(index_key_bytes1.clone(), ts1, doc1.clone()),
            ]
        );
        // Sub-interval also cached.
        let interval_gt_35 = Interval {
            start: Start::Included(values_to_bytes(&[Some(val!(35.0))]).into()),
            end: End::Unbounded,
        };
        assert_eq!(
            cache.get(index_id, &interval_gt_35, Order::Asc),
            vec![d(index_key_bytes2.clone(), ts2, doc2.clone())]
        );
        // Empty sub-interval also cached.
        let interval_eq_35 = Interval::prefix(values_to_bytes(&[Some(val!(35.0))]).into());
        assert_eq!(cache.get(index_id, &interval_eq_35, Order::Asc), vec![]);
        // Super-interval partially cached.
        let interval_gt_16 = Interval {
            start: Start::Included(values_to_bytes(&[Some(val!(16.0))]).into()),
            end: End::Unbounded,
        };
        assert_eq!(
            cache.get(index_id, &interval_gt_16, Order::Asc),
            vec![
                cache_miss(Interval {
                    start: interval_gt_16.start.clone(),
                    end: End::Excluded(values_to_bytes(&[Some(val!(18.0))]).into())
                }),
                d(index_key_bytes1.clone(), ts1, doc1.clone()),
                d(index_key_bytes2.clone(), ts2, doc2.clone()),
            ]
        );
        // Super-interval in reverse partially cached.
        assert_eq!(
            cache.get(index_id, &interval_gt_16, Order::Desc),
            vec![
                d(index_key_bytes2, ts2, doc2),
                d(index_key_bytes1, ts1, doc1),
                cache_miss(Interval {
                    start: interval_gt_16.start.clone(),
                    end: End::Excluded(values_to_bytes(&[Some(val!(18.0))]).into())
                }),
            ]
        );
        Ok(())
    }

    /// If the cache has a lot of points, we don't want to have a ton of small
    /// cache misses that require persistence queries. We restrict the number of
    /// persistence queries.
    #[test]
    fn sparse_cache() -> anyhow::Result<()> {
        let mut cache = DatabaseIndexSnapshotCache::new();
        let mut id_generator = TestIdGenerator::new();
        let index_id = id_generator.generate_internal();
        let ts = Timestamp::must(100);
        let mut make_doc = |age: f64| {
            let id = id_generator.generate(&"users".parse().unwrap());
            let doc =
                ResolvedDocument::new(id, CreationTime::ONE, assert_obj!("age" => age)).unwrap();
            let fields = vec!["age".parse().unwrap()];
            let index_key_bytes = doc
                .index_key(&fields, PersistenceVersion::default())
                .into_bytes();
            cache.populate(index_id, index_key_bytes.clone(), ts, doc.clone());
            (index_key_bytes, doc)
        };
        let (index_key1, doc1) = make_doc(30.0);
        let (index_key2, doc2) = make_doc(35.0);
        let (index_key3, doc3) = make_doc(40.0);
        let _ = make_doc(45.0);
        let _ = make_doc(50.0);
        let interval_gt_18 = Interval {
            start: Start::Included(values_to_bytes(&[Some(val!(18.0))]).into()),
            end: End::Unbounded,
        };
        let d = DatabaseIndexSnapshotCacheResult::Document;
        let cache_miss = DatabaseIndexSnapshotCacheResult::CacheMiss;
        assert_eq!(
            cache.get(index_id, &interval_gt_18, Order::Asc),
            vec![
                cache_miss(Interval {
                    start: interval_gt_18.start.clone(),
                    end: End::Excluded(index_key1.clone().into()),
                }),
                d(index_key1.clone(), ts, doc1),
                cache_miss(Interval {
                    start: Start::Included(BinaryKey::from(index_key1).increment().unwrap()),
                    end: End::Excluded(index_key2.clone().into()),
                }),
                d(index_key2.clone(), ts, doc2),
                cache_miss(Interval {
                    start: Start::Included(BinaryKey::from(index_key2).increment().unwrap()),
                    end: End::Excluded(index_key3.clone().into()),
                }),
                d(index_key3.clone(), ts, doc3),
                cache_miss(Interval {
                    start: Start::Included(BinaryKey::from(index_key3).increment().unwrap()),
                    end: End::Unbounded,
                }),
            ]
        );
        Ok(())
    }
}
