use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    os::unix::fs::MetadataExt,
    path::{
        Path,
        PathBuf,
    },
    sync::Arc,
};

use anyhow::Context;
use common::{
    bootstrap_model::index::text_index::FragmentedTextSegment,
    bounded_thread_pool::BoundedThreadPool,
    id_tracker::StaticIdTracker,
    persistence::DocumentRevisionStream,
    runtime::{
        try_join_buffer_unordered,
        Runtime,
    },
    types::ObjectKey,
};
use futures::TryStreamExt;
use storage::Storage;
use tantivy::{
    directory::MmapDirectory,
    fastfield::AliveBitSet,
    DocAddress,
    DocId,
    Index,
    IndexBuilder,
    SingleSegmentIndexWriter,
    Term,
};
use tempfile::TempDir;
use text_search::tracker::{
    load_alive_bitset,
    MemoryDeletionTracker,
    SearchMemoryIdTracker,
    SegmentTermMetadata,
};
use value::InternalId;

use crate::{
    archive::cache::ArchiveCacheManager,
    constants::CONVEX_EN_TOKENIZER,
    convex_en,
    disk_index::{
        download_single_file_zip,
        upload_single_file,
        upload_text_segment,
    },
    metrics::{
        log_compacted_segment_size_bytes,
        log_text_document_indexed,
        SearchType,
    },
    searcher::{
        FragmentedTextStorageKeys,
        SegmentTermMetadataFetcher,
        TermDeletionsByField,
    },
    SearchFileType,
    TantivySearchIndexSchema,
};

/// The maximum size of a segment in bytes. 10MB.
const SEGMENT_MAX_SIZE_BYTES: usize = 10_000_000;

pub(crate) const ID_TRACKER_PATH: &str = "id_tracker";
pub(crate) const ALIVE_BITSET_PATH: &str = "tantivy_alive_bitset";
pub(crate) const DELETED_TERMS_PATH: &str = "deleted_terms";

pub struct NewTextSegment {
    pub paths: TextSegmentPaths,
    /// The total number of indexed documents in this segment, including
    /// documents that were added and then marked as deleted.
    pub num_indexed_documents: u64,
    pub size_bytes_total: u64,
}

#[derive(Clone)]
pub struct TextSegmentPaths {
    pub index_path: PathBuf,
    pub id_tracker_path: PathBuf,
    pub alive_bit_set_path: PathBuf,
    pub deleted_terms_path: PathBuf,
}

pub struct UpdatableTextSegment {
    id_tracker: StaticIdTracker,
    pub(crate) deletion_tracker: MemoryDeletionTracker,
    original: FragmentedTextSegment,
    /// The number of files deleted since this struct was created (not the total
    /// number of deletes in the segment).
    num_deletes_in_updated: u64,
}

impl UpdatableTextSegment {
    pub fn segment_key(&self) -> &ObjectKey {
        &self.original.segment_key
    }

    fn delete(&mut self, tantivy_id: DocId) {
        self.num_deletes_in_updated += 1;
        self.deletion_tracker.alive_bitset.remove(tantivy_id);
    }

    fn update_term_metadata(
        &mut self,
        segment_term_metadata: SegmentTermMetadata,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.num_deletes_in_updated > 0,
            "Trying to update term metadata without any new deletes?",
        );
        self.deletion_tracker
            .update_term_metadata(segment_term_metadata);
        Ok(())
    }

    fn get_tantivy_id(&self, convex_id: InternalId) -> Option<DocId> {
        if let Some(tantivy_id) = self.id_tracker.lookup(convex_id.0)
            && self.deletion_tracker.alive_bitset.contains(tantivy_id)
        {
            Some(tantivy_id)
        } else {
            None
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn load(paths: &TextSegmentPaths) -> anyhow::Result<Self> {
        let id_tracker = StaticIdTracker::load_from_path(&paths.id_tracker_path)?;
        let deletion_tracker =
            MemoryDeletionTracker::load(&paths.alive_bit_set_path, &paths.deleted_terms_path)?;
        Ok(UpdatableTextSegment {
            id_tracker,
            deletion_tracker,
            num_deletes_in_updated: 0,
            // TODO(sam): We should probably create this outside of this method, then pass it
            // through here. For now this is unused in these tests.
            original: FragmentedTextSegment {
                segment_key: "segment".try_into()?,
                id_tracker_key: "id_tracker".try_into()?,
                deleted_terms_table_key: "deleted_terms".try_into()?,
                alive_bitset_key: "bitset".try_into()?,
                num_indexed_documents: 0,
                num_deleted_documents: 0,
                id: "test_id".to_string(),
                size_bytes_total: 0,
            },
        })
    }

    pub async fn upload_metadata(
        self,
        storage: Arc<dyn Storage>,
    ) -> anyhow::Result<FragmentedTextSegment> {
        if self.num_deletes_in_updated == 0 {
            return Ok(self.original);
        }

        let mut bitset_buf = vec![];
        let mut deleted_terms_buf = vec![];
        self.deletion_tracker
            .write(&mut bitset_buf, &mut deleted_terms_buf)?;

        let mut bitset_slice = bitset_buf.as_slice();
        let upload_bitset = upload_single_file(
            &mut bitset_slice,
            "alive_bitset".to_string(),
            storage.clone(),
            SearchFileType::TextAliveBitset,
        );
        let mut deleted_terms_slice = deleted_terms_buf.as_slice();
        let upload_deleted_terms = upload_single_file(
            &mut deleted_terms_slice,
            "deleted_terms".to_string(),
            storage.clone(),
            SearchFileType::TextDeletedTerms,
        );
        let (alive_bitset_key, deleted_terms_table_key) =
            futures::try_join!(upload_bitset, upload_deleted_terms)?;
        Ok(FragmentedTextSegment {
            deleted_terms_table_key,
            alive_bitset_key,
            num_deleted_documents: self.original.num_deleted_documents
                + self.num_deletes_in_updated,
            ..self.original
        })
    }

    pub async fn download(
        original: FragmentedTextSegment,
        storage: Arc<dyn Storage>,
    ) -> anyhow::Result<Self> {
        // Temp dir is fine because we're loading these into memory immediately.
        let tmp_dir = TempDir::new()?;

        let id_tracker_path = tmp_dir.path().join(ID_TRACKER_PATH);
        download_single_file_zip(&original.id_tracker_key, &id_tracker_path, storage.clone())
            .await?;
        let id_tracker = StaticIdTracker::load_from_path(id_tracker_path)?;

        let alive_bitset_path = tmp_dir.path().join(ALIVE_BITSET_PATH);
        download_single_file_zip(
            &original.alive_bitset_key,
            &alive_bitset_path,
            storage.clone(),
        )
        .await?;
        let deleted_terms_path = tmp_dir.path().join(DELETED_TERMS_PATH);
        download_single_file_zip(
            &original.deleted_terms_table_key,
            &deleted_terms_path,
            storage,
        )
        .await?;

        let deletion_tracker =
            MemoryDeletionTracker::load(&alive_bitset_path, &deleted_terms_path)?;

        Ok(UpdatableTextSegment {
            id_tracker,
            deletion_tracker,
            original,
            num_deletes_in_updated: 0,
        })
    }
}

#[derive(Default)]
pub struct PreviousTextSegments(pub BTreeMap<ObjectKey, UpdatableTextSegment>);

impl PreviousTextSegments {
    /// Returns the index to the segment containing the live document and the
    /// tantivy id, if it exists
    fn segment_for_document(&self, convex_id: InternalId) -> Option<(ObjectKey, DocId)> {
        for (segment_key, segment) in self.0.iter() {
            if let Some(tantivy_id) = segment.get_tantivy_id(convex_id) {
                return Some((segment_key.clone(), tantivy_id));
            }
        }
        None
    }

    /// Deletes a document (if present) and returns the segment key of the
    /// segment that it was deleted from
    pub fn delete_document(&mut self, convex_id: InternalId) -> anyhow::Result<Option<ObjectKey>> {
        let Some((segment_key, tantivy_id)) = self.segment_for_document(convex_id) else {
            return Ok(None);
        };
        self.must_get_mut(&segment_key).delete(tantivy_id);
        Ok(Some(segment_key.clone()))
    }

    /// Returns the segment with the given key, panics if not found. This is
    /// useful if we got the segment key from one of these other methods so we
    /// know the key exists.
    fn must_get_mut(&mut self, segment_key: &ObjectKey) -> &mut UpdatableTextSegment {
        self.0.get_mut(segment_key).expect("Segment key not found")
    }

    fn update_term_deletion_metadata(
        &mut self,
        segments_term_metadata: Vec<(ObjectKey, SegmentTermMetadata)>,
    ) -> anyhow::Result<()> {
        for (segment_key, segment_term_metadata) in segments_term_metadata {
            self.must_get_mut(&segment_key)
                .update_term_metadata(segment_term_metadata)?;
        }
        Ok(())
    }
}

/// Builds a new segment from a stream of document revisions in descending
/// timestamp order, updating existing segments if a document was deleted.
///
/// Note the descending order requirement can be relaxed if the caller can
/// guarantee that no deletes will be present in the stream. A caller can do so
/// when providing this function with a stream from table iterator for example.
pub async fn build_new_segment<RT: Runtime>(
    rt: &RT,
    revision_stream: DocumentRevisionStream<'_>,
    tantivy_schema: TantivySearchIndexSchema,
    dir: &Path,
    previous_segments: &mut PreviousTextSegments,
    segment_term_metadata_fetcher: Arc<dyn SegmentTermMetadataFetcher>,
    search_storage: Arc<dyn Storage>,
) -> anyhow::Result<Option<NewTextSegment>> {
    let index_path = dir.join("index_path");
    std::fs::create_dir(&index_path)?;
    let index = IndexBuilder::new()
        .schema(tantivy_schema.schema.clone())
        .create_in_dir(&index_path)?;
    index
        .tokenizers()
        .register(CONVEX_EN_TOKENIZER, convex_en());
    let mut segment_writer = SingleSegmentIndexWriter::new(index, SEGMENT_MAX_SIZE_BYTES)?;
    let mut new_id_tracker = SearchMemoryIdTracker::default();
    futures::pin_mut!(revision_stream);
    // Keep track of the document IDs we've seen so we can check for duplicates.
    // We'll discard revisions to documents that we've already seen because we are
    // processing in reverse timestamp order.
    let mut document_ids_seen = BTreeSet::new();
    // Keep track of deletes that don't correspond to a document in another segment.
    // It must appear later in the stream
    let mut dangling_deletes = BTreeSet::new();

    let mut num_indexed_documents = 0;

    let mut is_at_least_one_document_indexed = false;
    let mut term_deletes_by_segment: BTreeMap<ObjectKey, TermDeletionsByField> = BTreeMap::new();

    while let Some(revision_pair) = revision_stream.try_next().await? {
        let convex_id = revision_pair.id.internal_id();
        // Skip documents we have already added to the segment, but update dangling
        // deletes
        if document_ids_seen.contains(&convex_id) {
            if revision_pair.document().is_some() && revision_pair.prev_document().is_none() {
                dangling_deletes.remove(&convex_id);
            }
            continue;
        }
        document_ids_seen.insert(convex_id);
        // Delete
        if let Some(prev_document) = revision_pair.prev_document() {
            if let Some(segment_key) =
                previous_segments.delete_document(prev_document.id().internal_id())?
            {
                let segment = previous_segments
                    .0
                    .get(&segment_key)
                    .context("Segment key not found")?;
                let terms = tantivy_schema.index_into_terms(prev_document)?;
                // Create a set of unique terms so we don't double count terms. The count is the
                // number of documents deleted containing the term.
                let term_set: BTreeSet<_> = terms.into_iter().map(Term::from).collect();
                for term in term_set {
                    let field = term.field();
                    let term_value = term.value_bytes().to_vec();
                    if let Some(term_values_and_delete_counts) =
                        term_deletes_by_segment.get_mut(&segment.original.segment_key)
                    {
                        term_values_and_delete_counts.increment(field, term_value);
                    } else {
                        let mut term_values_and_delete_counts = TermDeletionsByField::default();
                        term_values_and_delete_counts.increment(field, term_value);
                        term_deletes_by_segment.insert(
                            segment.original.segment_key.clone(),
                            term_values_and_delete_counts,
                        );
                    }
                }
            } else {
                // Add this document to dangling deletes because it was not present in other
                // segments, so it must be added further down in this stream.
                dangling_deletes.insert(convex_id);
            };
        }
        // Addition
        if let Some(new_document) = revision_pair.document() {
            is_at_least_one_document_indexed = true;
            num_indexed_documents += 1;
            dangling_deletes.remove(&convex_id);
            let tantivy_document =
                tantivy_schema.index_into_tantivy_document(new_document, revision_pair.ts());
            log_text_document_indexed(&tantivy_schema, &tantivy_document);
            let doc_id = segment_writer.add_document(tantivy_document)?;
            new_id_tracker.set_link(convex_id, doc_id)?;
        }
    }
    anyhow::ensure!(
        dangling_deletes.is_empty(),
        "Dangling deletes is not empty. A document was deleted that is not present in other \
         segments nor in this stream"
    );

    let segments_term_metadata = get_all_segment_term_metadata(
        rt,
        search_storage,
        segment_term_metadata_fetcher,
        term_deletes_by_segment,
    )
    .await?;
    previous_segments.update_term_deletion_metadata(segments_term_metadata)?;

    if !is_at_least_one_document_indexed {
        return Ok(None);
    }
    // Finalize the segment and write to trackers
    segment_writer.finalize()?;
    let new_deletion_tracker = MemoryDeletionTracker::new(new_id_tracker.num_ids() as u32);
    let alive_bit_set_path = dir.join(ALIVE_BITSET_PATH);
    let deleted_terms_path = dir.join(DELETED_TERMS_PATH);
    new_deletion_tracker.write_to_path(&alive_bit_set_path, &deleted_terms_path)?;
    let id_tracker_path = dir.join(ID_TRACKER_PATH);
    new_id_tracker.write(&id_tracker_path)?;
    let total_size_bytes = get_size(&index_path)?
        + get_size(&id_tracker_path)?
        + get_size(&alive_bit_set_path)?
        + get_size(&deleted_terms_path)?;
    let paths = TextSegmentPaths {
        index_path,
        id_tracker_path,
        alive_bit_set_path,
        deleted_terms_path,
    };
    Ok(Some(NewTextSegment {
        paths,
        num_indexed_documents,
        size_bytes_total: total_size_bytes,
    }))
}

fn get_size(path: &PathBuf) -> anyhow::Result<u64> {
    if path.is_file() {
        return Ok(path.metadata()?.size());
    }
    std::fs::read_dir(path)?.try_fold(0, |acc, curr| Ok(acc + get_size(&curr?.path())?))
}

async fn get_all_segment_term_metadata<RT: Runtime>(
    rt: &RT,
    storage: Arc<dyn Storage>,
    segment_term_metadata_fetcher: Arc<dyn SegmentTermMetadataFetcher>,
    term_deletes_by_segment: BTreeMap<ObjectKey, TermDeletionsByField>,
) -> anyhow::Result<Vec<(ObjectKey, SegmentTermMetadata)>> {
    let segment_term_metadata_futs = term_deletes_by_segment.into_iter().map(
        move |(segment_key, term_values_and_delete_counts)| {
            let segment_key = segment_key.clone();
            let segment_term_metadata_fetcher = segment_term_metadata_fetcher.clone();
            let storage = storage.clone();
            async move {
                let term_metadata = segment_term_metadata_fetcher
                    .clone()
                    .segment_term_metadata(
                        storage.clone(),
                        segment_key.clone(),
                        term_values_and_delete_counts,
                    )
                    .await?;
                anyhow::Ok::<(ObjectKey, SegmentTermMetadata)>((segment_key, term_metadata))
            }
        },
    );
    let segments_term_metadata: Vec<_> =
        try_join_buffer_unordered(rt, "text_term_metadata", segment_term_metadata_futs).await?;
    Ok(segments_term_metadata)
}

pub struct SearchSegmentForMerge {
    pub segment: Index,
    pub alive_bitset: AliveBitSet,
    pub id_tracker: StaticIdTracker,
}

pub async fn fetch_text_segment<RT: Runtime>(
    archive_cache: ArchiveCacheManager<RT>,
    storage: Arc<dyn Storage>,
    keys: impl Into<FragmentedTextStorageKeys>,
) -> anyhow::Result<TextSegmentPaths> {
    let keys = keys.into();

    let fetch_index_path = archive_cache.get(storage.clone(), &keys.segment, SearchFileType::Text);
    let fetch_id_tracker_path = archive_cache.get_single_file(
        storage.clone(),
        &keys.id_tracker,
        SearchFileType::TextIdTracker,
    );
    let fetch_alive_bitset_path = archive_cache.get_single_file(
        storage.clone(),
        &keys.alive_bitset,
        SearchFileType::TextAliveBitset,
    );
    let fetch_deleted_terms = archive_cache.get_single_file(
        storage.clone(),
        &keys.deleted_terms_table,
        SearchFileType::TextDeletedTerms,
    );

    let (index_path, id_tracker_path, alive_bit_set_path, deleted_terms_path) = futures::try_join!(
        fetch_index_path,
        fetch_id_tracker_path,
        fetch_alive_bitset_path,
        fetch_deleted_terms
    )?;

    Ok(TextSegmentPaths {
        index_path,
        id_tracker_path,
        alive_bit_set_path,
        deleted_terms_path,
    })
}

pub fn open_text_segment_for_merge(
    paths: TextSegmentPaths,
) -> anyhow::Result<SearchSegmentForMerge> {
    let id_tracker = StaticIdTracker::load_from_path(paths.id_tracker_path)?;
    let alive_bitset = load_alive_bitset(&paths.alive_bit_set_path)?;

    let index = Index::open_in_dir(paths.index_path)?;
    Ok(SearchSegmentForMerge {
        segment: index,
        alive_bitset,
        id_tracker,
    })
}
pub async fn fetch_compact_and_upload_text_segment<RT: Runtime>(
    rt: &RT,
    storage: Arc<dyn Storage>,
    cache: ArchiveCacheManager<RT>,
    blocking_thread_pool: BoundedThreadPool<RT>,
    segments: Vec<FragmentedTextStorageKeys>,
) -> anyhow::Result<FragmentedTextSegment> {
    let _storage = storage.clone();
    let opened_segments = try_join_buffer_unordered(
        rt,
        "text_segment_merge",
        segments.into_iter().map(move |segment| {
            let pool = blocking_thread_pool.clone();
            let storage = storage.clone();
            let cache = cache.clone();
            let segment = segment.clone();
            async move {
                let paths = fetch_text_segment(cache, storage, segment).await?;
                pool.execute(|| open_text_segment_for_merge(paths)).await?
            }
        }),
    )
    .await?;

    let dir = TempDir::new()?;
    let new_segment = merge_segments(opened_segments, dir.path()).await?;
    upload_text_segment(rt, _storage, new_segment).await
}

#[allow(dead_code)]
pub async fn merge_segments(
    search_segments: Vec<SearchSegmentForMerge>,
    dir: &Path,
) -> anyhow::Result<NewTextSegment> {
    let mut segments = vec![];
    let settings = search_segments
        .first()
        .context("Called merge_segments with an empty vec of segments")?
        .segment
        .settings()
        .clone();
    for s in &search_segments {
        let mut index_segments = s.segment.searchable_segments()?;
        anyhow::ensure!(
            index_segments.len() == 1,
            "Expected exactly one segment, found {}",
            index_segments.len()
        );
        segments.push(index_segments.pop().unwrap());
    }
    let alive_bitsets = search_segments
        .iter()
        .map(|s| Some(s.alive_bitset.clone()))
        .collect::<Vec<_>>();
    let total_alive = search_segments
        .iter()
        .fold(0, |acc, e| acc + e.alive_bitset.num_alive_docs());

    let index_dir = dir.join("index_dir");
    std::fs::create_dir(&index_dir)?;
    let mmap_directory = MmapDirectory::open(&index_dir)?;
    let (_merged_segment, id_mapping) =
        tantivy::merge_filtered_segments(&segments, settings, alive_bitsets, mmap_directory)?;
    anyhow::ensure!(
        total_alive == id_mapping.len(),
        "Total alive documents expected did not match merged segment id mapping"
    );
    let mut new_segment_id_tracker = SearchMemoryIdTracker::default();
    for (
        new_tantivy_id,
        DocAddress {
            segment_ord,
            doc_id: old_doc_id,
        },
    ) in id_mapping.iter_old_doc_addrs_enumerated()
    {
        let old_id_tracker = &search_segments[segment_ord as usize].id_tracker;
        let convex_id = old_id_tracker
            .get_convex_id(old_doc_id as usize)
            .with_context(|| {
                format!("Old id tracker for segment {segment_ord} is missing id {old_doc_id}")
            })?;
        new_segment_id_tracker.set_link(InternalId(convex_id), new_tantivy_id)?;
    }
    let num_docs = new_segment_id_tracker.num_ids();
    let id_tracker_path = dir.to_path_buf().join(ID_TRACKER_PATH);
    let num_indexed_documents = new_segment_id_tracker.num_ids() as u64;
    new_segment_id_tracker.write(&id_tracker_path)?;
    let tracker = MemoryDeletionTracker::new(num_docs as u32);
    let alive_bit_set_path = dir.to_path_buf().join(ALIVE_BITSET_PATH);
    let deleted_terms_path = dir.to_path_buf().join(DELETED_TERMS_PATH);
    tracker.write_to_path(&alive_bit_set_path, &deleted_terms_path)?;
    let size_bytes_total = get_size(&index_dir)?;
    log_compacted_segment_size_bytes(size_bytes_total, SearchType::Text);
    Ok(NewTextSegment {
        num_indexed_documents,
        paths: TextSegmentPaths {
            index_path: index_dir,
            id_tracker_path,
            alive_bit_set_path,
            deleted_terms_path,
        },
        size_bytes_total,
    })
}
