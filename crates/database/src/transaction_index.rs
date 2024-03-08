use std::{
    cmp::Ordering,
    collections::BTreeMap,
    sync::{
        Arc,
        OnceLock,
    },
};

use async_trait::async_trait;
use common::{
    bootstrap_model::index::{
        database_index::{
            DeveloperDatabaseIndexConfig,
            IndexedFields,
        },
        IndexConfig,
    },
    document::{
        DocumentUpdate,
        PackedDocument,
        ResolvedDocument,
    },
    index::{
        IndexKey,
        IndexKeyBytes,
    },
    interval::Interval,
    knobs::TRANSACTION_MAX_READ_SIZE_BYTES,
    query::{
        InternalSearch,
        Order,
        SearchVersion,
    },
    types::{
        DatabaseIndexUpdate,
        DatabaseIndexValue,
        IndexId,
        IndexName,
        TabletIndexName,
        WriteTimestamp,
    },
};
use indexing::{
    backend_in_memory_indexes::{
        index_not_a_database_index_error,
        DatabaseIndexSnapshot,
    },
    index_registry::{
        Index,
        IndexRegistry,
    },
};
use search::{
    query::RevisionWithKeys,
    CandidateRevision,
    QueryResults,
    SearchIndexManager,
    Searcher,
};
use storage::Storage;
use value::{
    DeveloperDocumentId,
    FieldPath,
};

use crate::{
    preloaded::PreloadedIndexRange,
    reads::TransactionReadSet,
    DEFAULT_PAGE_SIZE,
};

/// [`TransactionIndex`] is an index used by transactions.
/// It gets constructed from [`DatabaseIndexSnapshot`] and [`IndexRegistry`] at
/// a timestamp snapshot. It buffers the transaction pending index updates and
/// merges and overlays them on top of the snapshot to allow the transaction to
/// read its own writes.
pub struct TransactionIndex {
    // Metadata about existing indexes with any changes to the index tables applied. Note that
    // those changes are stored separately in `database_index_updates` and `search_index_updates`
    // in their underlying database writes too.
    index_registry: IndexRegistry,
    // Weather the index registry has been updates since the beginning of the transaction.
    index_registry_updated: bool,

    // Database indexes combine a base index snapshot in persistence with pending updates applied
    // in-memory.
    database_index_snapshot: DatabaseIndexSnapshot,
    database_index_updates: BTreeMap<IndexId, TransactionIndexMap>,

    // Similar to database indexes, text search indexes are implemented by applying pending updates
    // on top of the transaction base snapshot.
    search_index_snapshot: Arc<dyn TransactionSearchSnapshot>,
    search_index_updates: BTreeMap<IndexId, Vec<DocumentUpdate>>,
}

impl TransactionIndex {
    pub fn new(
        index_registry: IndexRegistry,
        database_index_snapshot: DatabaseIndexSnapshot,
        search_index_snapshot: Arc<dyn TransactionSearchSnapshot>,
    ) -> Self {
        Self {
            index_registry,
            index_registry_updated: false,
            database_index_snapshot,
            database_index_updates: BTreeMap::new(),
            search_index_snapshot,
            search_index_updates: BTreeMap::new(),
        }
    }

    pub fn index_registry(&self) -> &IndexRegistry {
        &self.index_registry
    }

    /// Range over a index including pending updates.
    /// `size_hint` provides an estimate of the number of rows to be
    /// streamed from the database.
    /// The returned vec may be larger or smaller than `size_hint` depending on
    /// pending writes.
    async fn range_no_deps(
        &mut self,
        index_name: &TabletIndexName,
        printable_index_name: &IndexName,
        interval: Interval,
        order: Order,
        size_hint: usize,
    ) -> anyhow::Result<(
        Vec<(IndexKeyBytes, ResolvedDocument, WriteTimestamp)>,
        Interval,
    )> {
        let snapshot = &mut self.database_index_snapshot;
        let index_registry = &self.index_registry;
        let database_index_updates = &self.database_index_updates;
        let pending_it = match index_registry.require_enabled(index_name, printable_index_name) {
            Ok(index) => database_index_updates.get(&index.id()),
            // Range queries on missing tables are allowed for system provided indexes.
            Err(_) if index_name.is_by_id_or_creation_time() => None,
            Err(e) => anyhow::bail!(e),
        }
        .map(|pending| pending.range(&interval))
        .into_iter()
        .flatten();
        let mut pending_it = order.apply(pending_it);
        let (results, remaining_interval) = snapshot
            .range(
                index_name.clone(),
                printable_index_name,
                interval,
                order,
                size_hint,
            )
            .await?;
        let mut snapshot_it = results.into_iter();

        let mut snapshot_next = snapshot_it.next();
        let mut pending_next = pending_it.next();
        let mut results = vec![];
        loop {
            match (snapshot_next, pending_next) {
                (
                    Some((snapshot_key, snapshot_ts, snapshot_doc)),
                    Some((pending_key, maybe_pending_doc)),
                ) => {
                    let cmp = match order {
                        Order::Asc => snapshot_key.cmp(&pending_key),
                        Order::Desc => pending_key.cmp(&snapshot_key),
                    };
                    match cmp {
                        Ordering::Less => {
                            results.push((
                                snapshot_key,
                                snapshot_doc,
                                WriteTimestamp::Committed(snapshot_ts),
                            ));
                            snapshot_next = snapshot_it.next();
                            pending_next = Some((pending_key, maybe_pending_doc));
                        },
                        Ordering::Equal => {
                            // The pending entry overwrites the snapshot one.
                            if let Some(pending_doc) = maybe_pending_doc {
                                results.push((pending_key, pending_doc, WriteTimestamp::Pending));
                            };
                            snapshot_next = snapshot_it.next();
                            pending_next = pending_it.next();
                        },
                        Ordering::Greater => {
                            if let Some(pending_doc) = maybe_pending_doc {
                                results.push((pending_key, pending_doc, WriteTimestamp::Pending));
                            };
                            snapshot_next = Some((snapshot_key, snapshot_ts, snapshot_doc));
                            pending_next = pending_it.next();
                        },
                    }
                },
                (Some((snapshot_key, snapshot_ts, snapshot_doc)), None) => {
                    results.push((
                        snapshot_key,
                        snapshot_doc,
                        WriteTimestamp::Committed(snapshot_ts),
                    ));
                    snapshot_next = snapshot_it.next();
                    pending_next = None;
                },
                (None, Some((pending_key, maybe_pending_doc))) => {
                    if let Some(pending_doc) = maybe_pending_doc {
                        results.push((pending_key, pending_doc, WriteTimestamp::Pending));
                    };
                    snapshot_next = None;
                    pending_next = pending_it.next();
                },
                (None, None) => break,
            }
        }
        Ok((results, remaining_interval))
    }

    pub async fn search(
        &mut self,
        reads: &mut TransactionReadSet,
        query: &InternalSearch,
        index_name: TabletIndexName,
        version: SearchVersion,
    ) -> anyhow::Result<Vec<(CandidateRevision, IndexKeyBytes)>> {
        // We do not allow modifying the index registry and performing a text search
        // in the same transaction. We could implement this by sending the index
        // updates in the search request, but there is no need to bother since we
        // don't yet have a use case of modifying an index metadata and performing
        // a text search in the same transaction.
        anyhow::ensure!(
            !self.index_registry_updated,
            "Text search and index registry update not allowed in the same transaction"
        );
        let index = self.require_enabled(reads, &index_name, &query.printable_index_name()?)?;
        let empty = vec![];
        let pending_updates = self.search_index_updates.get(&index.id).unwrap_or(&empty);
        let results = self
            .search_index_snapshot
            .search(&index, query, version, pending_updates)
            .await?;

        // TODO: figure out if we want to charge database bandwidth for reading search
        // index metadata once search is no longer beta

        // Record the query results in the read set.
        reads.record_search(index_name.clone(), results.reads);

        Ok(results.revisions_with_keys)
    }

    /// Returns the next page from the index range.
    /// NOTE: the caller must call reads.record_read_document for any
    /// documents yielded from the index scan.
    /// Returns the remaining interval that was skipped because of max_size or
    /// transaction size limits.
    pub async fn range(
        &mut self,
        reads: &mut TransactionReadSet,
        index_name: &TabletIndexName,
        printable_index_name: &IndexName,
        interval: &Interval,
        order: Order,
        max_size: usize,
    ) -> anyhow::Result<(
        Vec<(IndexKeyBytes, ResolvedDocument, WriteTimestamp)>,
        Interval,
    )> {
        let indexed_fields = match self.require_enabled(reads, index_name, printable_index_name) {
            Ok(index) => match index.metadata().config.clone() {
                IndexConfig::Database {
                    developer_config: DeveloperDatabaseIndexConfig { fields },
                    ..
                } => fields,
                _ => anyhow::bail!(index_not_a_database_index_error(printable_index_name)),
            },
            // Range queries on missing system tables are allowed.
            Err(_) if index_name.is_by_id() => IndexedFields::by_id(),
            Err(_) if index_name.is_creation_time() => IndexedFields::creation_time(),
            Err(e) => anyhow::bail!(e),
        };
        let (documents, interval_unfetched) = self
            .range_no_deps(
                index_name,
                printable_index_name,
                interval.clone(),
                order,
                // We use max_rows as size hint. We might receive more or less
                // due to pending deletes or inserts in the transaction.
                max_size,
            )
            .await?;
        let mut total_bytes = 0;
        let mut within_bytes_limit = true;
        let out: Vec<_> = documents
            .into_iter()
            .take(max_size)
            .take_while(|(_, document, _)| {
                within_bytes_limit = total_bytes < *TRANSACTION_MAX_READ_SIZE_BYTES;
                // Allow the query to exceed the limit by one document so the query
                // is guaranteed to make progress and probably fail.
                // Note system document limits are different, so a single document
                // can be larger than `TRANSACTION_MAX_READ_SIZE_BYTES`.
                total_bytes += document.size();
                within_bytes_limit
            })
            .collect();

        let mut interval_read = Interval::empty();
        let mut interval_unread = interval.clone();
        if out.len() < max_size && within_bytes_limit && interval_unfetched.is_empty() {
            // If we exhaust the query before hitting any early-termination condition,
            // put the entire range in the read set.
            interval_read = interval.clone();
            interval_unread = Interval::empty();
        } else if let Some((last_key, ..)) = out.last() {
            // If there is more in the query, split at the last key returned.
            (interval_read, interval_unread) = interval.split(last_key.clone(), order);
        }
        reads.record_indexed_directly(index_name.clone(), indexed_fields.clone(), interval_read)?;
        Ok((out, interval_unread))
    }

    pub async fn preload_index_range(
        &mut self,
        reads: &mut TransactionReadSet,
        tablet_index_name: &TabletIndexName,
        printable_index_name: &IndexName,
        interval: &Interval,
    ) -> anyhow::Result<PreloadedIndexRange> {
        let index = self.require_enabled(reads, tablet_index_name, printable_index_name)?;
        let IndexConfig::Database {
            developer_config: DeveloperDatabaseIndexConfig { ref fields, .. },
            ..
        } = index.metadata().config
        else {
            anyhow::bail!("{printable_index_name} isn't a database index");
        };
        let indexed_fields: Vec<FieldPath> = fields.clone().into();
        let indexed_field = indexed_fields[0].clone();
        anyhow::ensure!(indexed_fields.len() == 1);
        let mut remaining_interval = interval.clone();
        let mut preloaded = BTreeMap::new();
        while !remaining_interval.is_empty() {
            let (documents, new_remaining_interval) = self
                .range_no_deps(
                    tablet_index_name,
                    printable_index_name,
                    remaining_interval,
                    Order::Asc,
                    DEFAULT_PAGE_SIZE,
                )
                .await?;
            remaining_interval = new_remaining_interval;
            for (_, document, _) in documents {
                let key = document.value().0.get_path(&indexed_field).cloned();
                anyhow::ensure!(
                    preloaded.insert(key, document).is_none(),
                    "Index {printable_index_name:?} isn't unique",
                );
            }
        }
        // Since PreloadedIndexRange only permits looking up documents by the index
        // key, we don't need to record `interval` as a read dependency. Put another
        // way, even though we're reading all of the rows in `interval`, the layer
        // above is only allowed to do point queries against `index_name`.
        Ok(PreloadedIndexRange::new(
            printable_index_name.table().clone(),
            tablet_index_name.clone(),
            indexed_field,
            preloaded,
        ))
    }

    // TODO: Add precise error types to facilitate detecting which indexing errors
    // are the developer's fault or not.
    pub fn begin_update(
        &mut self,
        old_document: Option<ResolvedDocument>,
        new_document: Option<ResolvedDocument>,
    ) -> anyhow::Result<Update<'_>> {
        let mut registry = self.index_registry.clone();
        registry.update(old_document.as_ref(), new_document.as_ref())?;

        Ok(Update {
            index: self,
            deletion: old_document,
            insertion: new_document,
            registry,
        })
    }

    fn finish_update(
        &mut self,
        old_document: Option<ResolvedDocument>,
        new_document: Option<ResolvedDocument>,
    ) -> Vec<DatabaseIndexUpdate> {
        // Update the index registry first.
        let index_registry_updated = self
            .index_registry
            .apply_verified_update(old_document.as_ref(), new_document.as_ref());
        self.index_registry_updated |= index_registry_updated;

        // Then compute the index updates.
        let updates = self
            .index_registry
            .index_updates(old_document.as_ref(), new_document.as_ref());

        // Add the index updates to self.database_index_updates.
        for update in &updates {
            let new_value = match &update.value {
                DatabaseIndexValue::Deleted => None,
                DatabaseIndexValue::NonClustered(doc_id) => {
                    // The pending updates are clustered. Get the document
                    // from the update itself.
                    match new_document {
                        Some(ref doc) => {
                            assert_eq!(doc.id(), doc_id);
                            Some(doc.clone())
                        },
                        None => panic!("Unexpected index update: {:?}", update.value),
                    }
                },
            };
            self.database_index_updates
                .entry(update.index_id)
                .or_insert_with(TransactionIndexMap::new)
                .insert(update.key.clone(), new_value);
        }

        // If we are updating a document, the old and new ids must be the same.
        let document_id = new_document
            .as_ref()
            .map(|d| *d.id())
            .or(old_document.as_ref().map(|d| *d.id()));
        if let Some(id) = document_id {
            // Add the update to all affected text search indexes.
            for index in self
                .index_registry
                .search_indexes_by_table(&id.table().table_id)
            {
                self.search_index_updates
                    .entry(index.id)
                    .or_default()
                    .push(DocumentUpdate {
                        id,
                        old_document: old_document.clone(),
                        new_document: new_document.clone(),
                    });
            }
        }

        // Note that we do not update the vector index and we always read at the
        // base snapshot.

        updates
    }

    pub fn get_pending(
        &self,
        reads: &mut TransactionReadSet,
        index_name: &TabletIndexName,
    ) -> Option<&Index> {
        self._get(reads, || self.index_registry.get_pending(index_name))
    }

    pub fn get_enabled(
        &self,
        reads: &mut TransactionReadSet,
        index_name: &TabletIndexName,
    ) -> Option<&Index> {
        self._get(reads, || self.index_registry.get_enabled(index_name))
    }

    fn _get<'a>(
        &'a self,
        reads: &mut TransactionReadSet,
        getter: impl FnOnce() -> Option<&'a Index>,
    ) -> Option<&Index> {
        let result = getter();
        self.record_interval(reads, result);
        result
    }

    pub fn require_enabled(
        &self,
        reads: &mut TransactionReadSet,
        index_name: &TabletIndexName,
        printable_index_name: &IndexName,
    ) -> anyhow::Result<Index> {
        let result = self
            .index_registry
            .require_enabled(index_name, printable_index_name)?;
        self.record_interval(reads, Some(&result));
        Ok(result)
    }

    fn record_interval(&self, reads: &mut TransactionReadSet, index: Option<&Index>) {
        let index_table = self.index_registry.index_table();
        let interval = match index {
            // Note there is no _index.by_name index. In order for the
            // name->index mapping to depend only on index id, we rely
            // on index name being immutable.
            Some(index) => {
                let full_index_id = DeveloperDocumentId::new(index_table.table_number, index.id());
                let index_key = IndexKey::new(vec![], full_index_id);
                Interval::prefix(index_key.into_bytes().into())
            },
            // On a name lookup miss, depend on all indexes.
            None => Interval::all(),
        };
        reads.record_indexed_derived(
            TabletIndexName::by_id(index_table.table_id),
            IndexedFields::by_id(),
            interval,
        );
    }

    /// Returns the snapshot the transaction is based on ignoring any pending
    /// updates.
    pub fn base_snapshot(&self) -> &DatabaseIndexSnapshot {
        &self.database_index_snapshot
    }

    pub fn base_snapshot_mut(&mut self) -> &mut DatabaseIndexSnapshot {
        &mut self.database_index_snapshot
    }
}

#[derive(Debug)]
pub struct TransactionIndexMap {
    /// Unlike IndexMap we can simply use BTreeMap since the TransactionIndexMap
    /// does not get clones. The value needs to be Option<Document> since we
    /// need to distinguish between objects deleted within the transaction
    /// from objects that never existed.
    inner: BTreeMap<Vec<u8>, Option<PackedDocument>>,
}

impl TransactionIndexMap {
    pub fn new() -> Self {
        Self {
            inner: BTreeMap::new(),
        }
    }

    pub fn range(
        &self,
        interval: &Interval,
    ) -> impl DoubleEndedIterator<Item = (IndexKeyBytes, Option<ResolvedDocument>)> + '_ {
        self.inner
            .range(interval)
            .map(|(k, v)| (IndexKeyBytes(k.clone()), v.as_ref().map(|v| v.unpack())))
    }

    pub fn insert(&mut self, k: IndexKey, v: Option<ResolvedDocument>) {
        self.inner
            .insert(k.into_bytes().0, v.map(PackedDocument::pack));
    }
}

pub struct Update<'a> {
    index: &'a mut TransactionIndex,

    deletion: Option<ResolvedDocument>,
    insertion: Option<ResolvedDocument>,
    registry: IndexRegistry,
}

impl<'a> Update<'a> {
    pub fn apply(self) -> Vec<DatabaseIndexUpdate> {
        self.index.finish_update(self.deletion, self.insertion)
    }

    pub fn registry(&self) -> &IndexRegistry {
        &self.registry
    }
}

#[async_trait]
pub trait TransactionSearchSnapshot: Send + Sync + 'static {
    // Search at the given snapshot after applying the given writes.
    async fn search(
        &self,
        index: &Index,
        search: &InternalSearch,
        version: SearchVersion,
        // Note that we have to send the writes since we maintain an extremely high
        // bar of determinism - we expect the exact same result regardless if you
        // perform a query from a mutation with some pending writes, or a query after the
        // writes have been committed to the database. The easiest way to achieve
        // this is to send all pending writes back to the backend. This should be fine
        // in practice since mutations with a lot of writes *and* a lot searches
        // should be rare.
        // As a potential future optimization, we could try to make the caller much
        // more coupled with the search algorithm and require it to send bm25 statistics
        // diff, top fuzzy search suggestions and other search specific properties derived
        // from the writes. Alternatively, we could only do subset of that and relax the
        // determinism requirement since we don't really need to have deterministic between
        // search calls in mutations and search calls in queries, and if anyone relies on
        // this they will get random differences due to parallel writes that alter the
        // statistics anyway.
        pending_updates: &Vec<DocumentUpdate>,
    ) -> anyhow::Result<QueryResults>;
}

#[derive(Clone)]
pub struct SearchIndexManagerSnapshot {
    index_registry: IndexRegistry,
    search_indexes: SearchIndexManager,

    searcher: Arc<dyn Searcher>,
    search_storage: Arc<OnceLock<Arc<dyn Storage>>>,
}

impl SearchIndexManagerSnapshot {
    pub fn new(
        index_registry: IndexRegistry,
        search_indexes: SearchIndexManager,
        searcher: Arc<dyn Searcher>,
        search_storage: Arc<OnceLock<Arc<dyn Storage>>>,
    ) -> Self {
        Self {
            index_registry,
            search_indexes,
            searcher,
            search_storage,
        }
    }

    // Applies the writes to the base snapshot and returns the new snapshot.
    fn snapshot_with_updates(
        &self,
        pending_updates: &Vec<DocumentUpdate>,
    ) -> anyhow::Result<SearchIndexManager> {
        let mut search_indexes = self.search_indexes.clone();
        for DocumentUpdate {
            id: _,
            old_document,
            new_document,
        } in pending_updates
        {
            search_indexes.update(
                &self.index_registry,
                old_document.as_ref(),
                new_document.as_ref(),
                WriteTimestamp::Pending,
            )?;
        }
        Ok(search_indexes)
    }

    fn search_storage(&self) -> Arc<dyn Storage> {
        self.search_storage
            .get()
            .expect("search_storage not initialized")
            .clone()
    }

    pub async fn search_with_compiled_query(
        &self,
        index: &Index,
        printable_index_name: &IndexName,
        query: pb::searchlight::TextQuery,
        pending_updates: &Vec<DocumentUpdate>,
    ) -> anyhow::Result<RevisionWithKeys> {
        let search_indexes_snapshot = self.snapshot_with_updates(pending_updates)?;
        search_indexes_snapshot
            .search_with_compiled_query(
                index,
                printable_index_name,
                query,
                self.searcher.clone(),
                self.search_storage(),
            )
            .await
    }
}

#[async_trait]
impl TransactionSearchSnapshot for SearchIndexManagerSnapshot {
    async fn search(
        &self,
        index: &Index,
        search: &InternalSearch,
        version: SearchVersion,
        pending_updates: &Vec<DocumentUpdate>,
    ) -> anyhow::Result<QueryResults> {
        let search_indexes_snapshot = self.snapshot_with_updates(pending_updates)?;
        search_indexes_snapshot
            .search(
                index,
                search,
                self.searcher.clone(),
                self.search_storage(),
                version,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        str::FromStr,
        sync::{
            Arc,
            OnceLock,
        },
    };

    use common::{
        bootstrap_model::index::{
            database_index::IndexedFields,
            IndexMetadata,
            TabletIndexMetadata,
            INDEX_TABLE,
        },
        document::{
            CreationTime,
            ResolvedDocument,
        },
        index::IndexKey,
        interval::{
            BinaryKey,
            End,
            Interval,
            Start,
        },
        persistence::{
            now_ts,
            ConflictStrategy,
            Persistence,
            RepeatablePersistence,
        },
        query::Order,
        testing::{
            TestIdGenerator,
            TestPersistence,
        },
        types::{
            unchecked_repeatable_ts,
            IndexName,
            PersistenceVersion,
            TableName,
            TabletIndexName,
            Timestamp,
            WriteTimestamp,
        },
        value::ResolvedDocumentId,
    };
    use indexing::{
        backend_in_memory_indexes::{
            BackendInMemoryIndexes,
            DatabaseIndexSnapshot,
        },
        index_registry::IndexRegistry,
    };
    use runtime::prod::ProdRuntime;
    use search::{
        searcher::InProcessSearcher,
        SearchIndexManager,
    };
    use storage::{
        LocalDirStorage,
        Storage,
    };
    use value::assert_obj;

    use super::SearchIndexManagerSnapshot;
    use crate::{
        reads::TransactionReadSet,
        text_search_bootstrap::bootstrap_search,
        transaction_index::TransactionIndex,
        FollowerRetentionManager,
    };

    fn next_document_id(
        id_generator: &mut TestIdGenerator,
        table_name: &str,
    ) -> anyhow::Result<ResolvedDocumentId> {
        Ok(id_generator.generate(&TableName::from_str(table_name)?))
    }

    fn gen_index_document(
        id_generator: &mut TestIdGenerator,
        metadata: TabletIndexMetadata,
    ) -> anyhow::Result<ResolvedDocument> {
        let index_id = id_generator.generate(&INDEX_TABLE);
        ResolvedDocument::new(index_id, CreationTime::ONE, metadata.try_into()?)
    }

    async fn bootstrap_index(
        id_generator: &mut TestIdGenerator,
        mut indexes: Vec<TabletIndexMetadata>,
        persistence: RepeatablePersistence,
    ) -> anyhow::Result<(
        IndexRegistry,
        BackendInMemoryIndexes,
        SearchIndexManager,
        BTreeMap<TabletIndexName, ResolvedDocumentId>,
    )> {
        let mut index_id_by_name = BTreeMap::new();
        let mut index_documents = BTreeMap::new();

        let index_table = id_generator.table_id(&INDEX_TABLE).table_id;
        // Add the _index.by_id index.
        indexes.push(IndexMetadata::new_enabled(
            TabletIndexName::by_id(index_table),
            IndexedFields::by_id(),
        ));
        let ts = Timestamp::MIN;
        for metadata in indexes {
            let doc = gen_index_document(id_generator, metadata.clone())?;
            index_id_by_name.insert(metadata.name.clone(), *doc.id());
            index_documents.insert(*doc.id(), (ts, doc));
        }

        let index_registry = IndexRegistry::bootstrap(
            id_generator,
            index_documents.values().map(|(_, d)| d),
            PersistenceVersion::default(),
        )?;
        let index = BackendInMemoryIndexes::bootstrap(&index_registry, index_documents, ts)?;

        let (indexes, version) =
            bootstrap_search(&index_registry, &persistence, id_generator).await?;
        let search = SearchIndexManager::from_bootstrap(indexes, version);

        Ok((index_registry, index, search, index_id_by_name))
    }

    #[convex_macro::prod_rt_test]
    async fn test_transaction_index_missing_index(rt: ProdRuntime) -> anyhow::Result<()> {
        let mut id_generator = TestIdGenerator::new();

        let persistence = Box::new(TestPersistence::new());
        let retention_manager =
            Arc::new(FollowerRetentionManager::new(rt.clone(), persistence.clone()).await?);

        // Create a transactions with `by_name` index missing before the transaction
        // started.
        let rp = RepeatablePersistence::new(
            Box::new(TestPersistence::new()),
            unchecked_repeatable_ts(Timestamp::must(1000)),
            retention_manager,
        );
        let ps = rp.read_snapshot(unchecked_repeatable_ts(Timestamp::must(1000)))?;

        let table_id = id_generator.table_id(&"messages".parse()?).table_id;
        let messages_by_name = TabletIndexName::new(table_id, "by_name".parse()?)?;
        let printable_messages_by_name = IndexName::new("messages".parse()?, "by_name".parse()?)?;
        let (index_registry, inner, search, _) = bootstrap_index(
            &mut id_generator,
            vec![IndexMetadata::new_enabled(
                TabletIndexName::by_id(table_id),
                IndexedFields::by_id(),
            )],
            rp,
        )
        .await?;

        let mut reads = TransactionReadSet::new();
        let searcher = Arc::new(InProcessSearcher::new(rt.clone()).await?);
        let search_storage = Arc::new(LocalDirStorage::new(rt)?);
        let mut index = TransactionIndex::new(
            index_registry.clone(),
            DatabaseIndexSnapshot::new(
                index_registry.clone(),
                Arc::new(inner),
                id_generator.clone(),
                ps,
            ),
            Arc::new(SearchIndexManagerSnapshot::new(
                index_registry.clone(),
                search,
                searcher.clone(),
                Arc::new(OnceLock::from(search_storage as Arc<dyn Storage>)),
            )),
        );

        // Query the missing index. It should return an error because index is missing.
        {
            let result = index
                .range(
                    &mut reads,
                    &messages_by_name,
                    &printable_messages_by_name,
                    &Interval::all(),
                    Order::Asc,
                    100,
                )
                .await;
            assert!(result.is_err());
            match result {
                Ok(_) => panic!("Should have failed!"),
                Err(ref err) => {
                    assert!(
                        format!("{:?}", err).contains("Index messages.by_name not found."),
                        "Actual: {err:?}"
                    )
                },
            };
        }

        // Add the index. It should start returning errors since the index was not
        // backfilled at the snapshot.
        let by_name_metadata =
            IndexMetadata::new_enabled(messages_by_name.clone(), vec!["name".parse()?].try_into()?);
        let by_name = gen_index_document(&mut id_generator, by_name_metadata)?;
        index.begin_update(None, Some(by_name))?.apply();

        let result = index
            .range(
                &mut reads,
                &messages_by_name,
                &printable_messages_by_name,
                &Interval::all(),
                Order::Asc,
                100,
            )
            .await;
        assert!(result.is_err());
        match result {
            Ok(_) => panic!("Should have failed!"),
            Err(ref err) => {
                assert!(
                    format!("{:?}", err).contains("Index messages.by_name not found."),
                    "Actual: {err:?}"
                )
            },
        };

        Ok(())
    }

    #[convex_macro::prod_rt_test]
    async fn test_transaction_index_missing_table(rt: ProdRuntime) -> anyhow::Result<()> {
        let mut id_generator = TestIdGenerator::new();
        let table_id = id_generator.table_id(&"messages".parse()?).table_id;
        let by_id = TabletIndexName::by_id(table_id);
        let printable_by_id = IndexName::by_id("messages".parse()?);
        let by_name = TabletIndexName::new(table_id, "by_name".parse()?)?;
        let printable_by_name = IndexName::new("messages".parse()?, "by_name".parse()?)?;

        // Create a transactions with table missing before the transaction started.
        let persistence = Box::new(TestPersistence::new());
        let persistence_version = persistence.reader().version();
        let retention_manager =
            Arc::new(FollowerRetentionManager::new(rt.clone(), persistence.clone()).await?);
        let rp = RepeatablePersistence::new(
            persistence,
            unchecked_repeatable_ts(Timestamp::must(1000)),
            retention_manager,
        );
        let ps = rp.read_snapshot(unchecked_repeatable_ts(Timestamp::must(1000)))?;

        let (index_registry, inner, search, _) =
            bootstrap_index(&mut id_generator, vec![], rp).await?;

        let mut reads = TransactionReadSet::new();
        let searcher = Arc::new(InProcessSearcher::new(rt.clone()).await?);
        let search_storage = Arc::new(LocalDirStorage::new(rt)?);
        let mut index = TransactionIndex::new(
            index_registry.clone(),
            DatabaseIndexSnapshot::new(
                index_registry.clone(),
                Arc::new(inner),
                id_generator.clone(),
                ps,
            ),
            Arc::new(SearchIndexManagerSnapshot::new(
                index_registry.clone(),
                search,
                searcher.clone(),
                Arc::new(OnceLock::from(search_storage as Arc<dyn Storage>)),
            )),
        );

        // Query the missing table using table scan index. It should return no results.
        let (results, remaining_interval) = index
            .range(
                &mut reads,
                &by_id,
                &printable_by_id,
                &Interval::all(),
                Order::Asc,
                100,
            )
            .await?;
        assert!(remaining_interval.is_empty());
        assert!(results.is_empty());

        // Query by any other index should return an error.
        {
            let result = index
                .range(
                    &mut reads,
                    &by_name,
                    &printable_by_name,
                    &Interval::all(),
                    Order::Asc,
                    100,
                )
                .await;
            assert!(result.is_err());
            match result {
                Ok(_) => panic!("Should have failed!"),
                Err(ref err) => {
                    assert!(format!("{:?}", err).contains("Index messages.by_name not found."),)
                },
            };
        }

        // Add the table scan index. It should still give no results.
        let metadata = IndexMetadata::new_enabled(by_id.clone(), IndexedFields::by_id());
        let by_id_index = gen_index_document(&mut id_generator, metadata.clone())?;
        index.begin_update(None, Some(by_id_index))?.apply();

        let (results, remaining_interval) = index
            .range(
                &mut reads,
                &by_id,
                &printable_by_id,
                &Interval::all(),
                Order::Asc,
                100,
            )
            .await?;
        assert!(remaining_interval.is_empty());
        assert!(results.is_empty());

        // Add a document and make sure we see it.
        let doc = ResolvedDocument::new(
            next_document_id(&mut id_generator, "messages")?,
            CreationTime::ONE,
            assert_obj!(
                "content" => "hello there!",
            ),
        )?;
        index.begin_update(None, Some(doc.clone()))?.apply();
        let (result, remaining_interval) = index
            .range(
                &mut reads,
                &by_id,
                &printable_by_id,
                &Interval::all(),
                Order::Asc,
                100,
            )
            .await?;
        assert_eq!(
            result,
            vec![(
                doc.index_key(&IndexedFields::by_id()[..], persistence_version)
                    .into_bytes(),
                doc,
                WriteTimestamp::Pending
            )],
        );
        assert!(remaining_interval.is_empty());

        Ok(())
    }

    #[convex_macro::prod_rt_test]
    async fn test_transaction_index_merge(rt: ProdRuntime) -> anyhow::Result<()> {
        let mut id_generator = TestIdGenerator::new();
        let by_id_fields = vec![];
        let by_name_fields = vec!["name".parse()?];
        let now0 = now_ts(Timestamp::MIN, &rt)?;
        let mut ps = Box::new(TestPersistence::new());
        let persistence_version = ps.reader().version();
        let retention_manager =
            Arc::new(FollowerRetentionManager::new(rt.clone(), ps.clone()).await?);
        let rp = RepeatablePersistence::new(
            ps.reader(),
            unchecked_repeatable_ts(now0),
            retention_manager.clone(),
        );
        let index_table_id = id_generator.table_id(&"_index".parse()?).table_id;
        let table: TableName = "users".parse()?;
        let table_id = id_generator.table_id(&table).table_id;
        let by_id = TabletIndexName::by_id(table_id);
        let printable_by_id = IndexName::by_id(table.clone());
        let by_name = TabletIndexName::new(table_id, "by_name".parse()?)?;
        let printable_by_name = IndexName::new(table.clone(), "by_name".parse()?)?;
        let (mut index_registry, mut index, search, index_ids) = bootstrap_index(
            &mut id_generator,
            vec![
                IndexMetadata::new_enabled(by_id.clone(), by_id_fields.clone().try_into()?),
                IndexMetadata::new_enabled(by_name.clone(), by_name_fields.clone().try_into()?),
            ],
            rp,
        )
        .await?;

        async fn add(
            index_registry: &mut IndexRegistry,
            index: &mut BackendInMemoryIndexes,
            ps: &mut TestPersistence,
            ts: Timestamp,
            doc: ResolvedDocument,
        ) -> anyhow::Result<()> {
            index_registry.update(None, Some(&doc))?;
            let index_updates = index.update(index_registry, ts, None, Some(doc.clone()));
            ps.write(
                vec![(ts, doc.id_with_table_id(), Some(doc.clone()))],
                index_updates.into_iter().map(|u| (ts, u)).collect(),
                ConflictStrategy::Error,
            )
            .await?;
            Ok(())
        }

        // Add "Alice", "Bob" and "Zack"
        let alice = ResolvedDocument::new(
            next_document_id(&mut id_generator, "users")?,
            CreationTime::ONE,
            assert_obj!(
                "name" => "alice",
            ),
        )?;
        let now1 = now0.succ()?;
        add(
            &mut index_registry,
            &mut index,
            &mut ps,
            now1,
            alice.clone(),
        )
        .await?;
        let bob = ResolvedDocument::new(
            next_document_id(&mut id_generator, "users")?,
            CreationTime::ONE,
            assert_obj!(
                "name" => "bob",
            ),
        )?;
        let now2 = now1.succ()?;
        add(&mut index_registry, &mut index, &mut ps, now2, bob.clone()).await?;
        let zack = ResolvedDocument::new(
            next_document_id(&mut id_generator, "users")?,
            CreationTime::ONE,
            assert_obj!(
                "name" => "zack",
            ),
        )?;
        let now3 = now2.succ()?;
        add(&mut index_registry, &mut index, &mut ps, now3, zack.clone()).await?;

        let by_id_index = *(index_ids.get(&by_id).unwrap());
        id_generator.write_tables(ps.box_clone()).await?;

        let now4 = now3.succ()?;
        // Start a transaction, add "David" and delete "Bob"
        let ps = RepeatablePersistence::new(ps, unchecked_repeatable_ts(now4), retention_manager)
            .read_snapshot(unchecked_repeatable_ts(now4))?;

        let mut reads = TransactionReadSet::new();
        let searcher = Arc::new(InProcessSearcher::new(rt.clone()).await?);
        let search_storage = Arc::new(LocalDirStorage::new(rt.clone())?);
        let mut index = TransactionIndex::new(
            index_registry.clone(),
            DatabaseIndexSnapshot::new(
                index_registry.clone(),
                Arc::new(index),
                id_generator.clone(),
                ps,
            ),
            Arc::new(SearchIndexManagerSnapshot::new(
                index_registry.clone(),
                search,
                searcher.clone(),
                Arc::new(OnceLock::from(search_storage as Arc<dyn Storage>)),
            )),
        );
        let david = ResolvedDocument::new(
            next_document_id(&mut id_generator, "users")?,
            CreationTime::ONE,
            assert_obj!("name" => "david"),
        )?;
        index.begin_update(None, Some(david.clone()))?.apply();
        index.begin_update(Some(bob), None)?.apply();

        // Query by id
        let (results, remaining_interval) = index
            .range(
                &mut reads,
                &by_id,
                &printable_by_id,
                &Interval::all(),
                Order::Asc,
                100,
            )
            .await?;
        assert!(remaining_interval.is_empty());
        assert_eq!(
            results,
            vec![
                (
                    alice
                        .index_key(&by_id_fields[..], persistence_version)
                        .into_bytes(),
                    alice.clone(),
                    WriteTimestamp::Committed(now1)
                ),
                (
                    zack.index_key(&by_id_fields[..], persistence_version)
                        .into_bytes(),
                    zack.clone(),
                    WriteTimestamp::Committed(now3)
                ),
                (
                    david
                        .index_key(&by_id_fields[..], persistence_version)
                        .into_bytes(),
                    david.clone(),
                    WriteTimestamp::Pending
                ),
            ]
        );
        let mut expected_reads = TransactionReadSet::new();
        expected_reads.record_indexed_derived(
            TabletIndexName::by_id(index_table_id),
            IndexedFields::by_id(),
            Interval::prefix(
                IndexKey::new(vec![], by_id_index.into())
                    .into_bytes()
                    .into(),
            ),
        );
        expected_reads.record_indexed_directly(by_id, IndexedFields::by_id(), Interval::all())?;
        assert_eq!(reads, expected_reads);
        // Query by name in ascending order
        let (results, remaining_interval) = index
            .range(
                &mut reads,
                &by_name,
                &printable_by_name,
                &Interval::all(),
                Order::Asc,
                100,
            )
            .await?;
        assert!(remaining_interval.is_empty());
        assert_eq!(
            results,
            vec![
                (
                    alice
                        .index_key(&by_name_fields[..], persistence_version)
                        .into_bytes(),
                    alice.clone(),
                    WriteTimestamp::Committed(now1)
                ),
                (
                    david
                        .index_key(&by_name_fields[..], persistence_version)
                        .into_bytes(),
                    david.clone(),
                    WriteTimestamp::Pending
                ),
                (
                    zack.index_key(&by_name_fields[..], persistence_version)
                        .into_bytes(),
                    zack.clone(),
                    WriteTimestamp::Committed(now3)
                ),
            ]
        );
        // Query by name in ascending order with limit=2.
        // Returned remaining interval should be ("david", unbounded).
        let (results, remaining_interval) = index
            .range(
                &mut reads,
                &by_name,
                &printable_by_name,
                &Interval::all(),
                Order::Asc,
                2,
            )
            .await?;
        assert_eq!(
            remaining_interval.start,
            Start::Included(
                BinaryKey::from(
                    david
                        .index_key(&by_name_fields[..], persistence_version)
                        .into_bytes()
                )
                .increment()
                .unwrap()
            )
        );
        assert_eq!(remaining_interval.end, End::Unbounded);
        assert_eq!(
            results,
            vec![
                (
                    alice
                        .index_key(&by_name_fields[..], persistence_version)
                        .into_bytes(),
                    alice.clone(),
                    WriteTimestamp::Committed(now1)
                ),
                (
                    david
                        .index_key(&by_name_fields[..], persistence_version)
                        .into_bytes(),
                    david.clone(),
                    WriteTimestamp::Pending
                ),
            ]
        );

        // Query by name in descending order
        let (result, remaining_interval) = index
            .range(
                &mut reads,
                &by_name,
                &printable_by_name,
                &Interval::all(),
                Order::Desc,
                100,
            )
            .await?;
        assert!(remaining_interval.is_empty());
        assert_eq!(
            result,
            vec![
                (
                    zack.index_key(&by_name_fields[..], persistence_version)
                        .into_bytes(),
                    zack,
                    WriteTimestamp::Committed(now3)
                ),
                (
                    david
                        .index_key(&by_name_fields[..], persistence_version)
                        .into_bytes(),
                    david,
                    WriteTimestamp::Pending
                ),
                (
                    alice
                        .index_key(&by_name_fields[..], persistence_version)
                        .into_bytes(),
                    alice,
                    WriteTimestamp::Committed(now1)
                ),
            ]
        );

        Ok(())
    }
}
