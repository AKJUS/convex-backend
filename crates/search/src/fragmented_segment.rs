use std::{
    path::PathBuf,
    sync::Arc,
};

use common::{
    bootstrap_model::index::vector_index::FragmentedVectorSegment,
    bounded_thread_pool::BoundedThreadPool,
    codel_queue::{
        new_codel_queue_async,
        CoDelQueueSender,
        ExpiredInQueue,
        QueueFull,
    },
    deleted_bitset::DeletedBitset,
    errors::report_error,
    id_tracker::StaticIdTracker,
    runtime::Runtime,
    types::ObjectKey,
};
use futures::{
    stream,
    FutureExt,
    Stream,
    StreamExt,
    TryStreamExt,
};
use itertools::Itertools;
use qdrant_segment::{
    id_tracker::IdTracker,
    types::ExtendedPointId,
};
use storage::Storage;
use tempfile::TempDir;
use vector::{
    id_tracker::VectorStaticIdTracker,
    qdrant_segments::{
        load_disk_segment,
        merge_disk_segments_hnsw,
        UntarredDiskSegmentPaths,
    },
    PreviousSegment,
};

use crate::{
    archive::cache::ArchiveCacheManager,
    disk_index::{
        download_single_file_zip,
        upload_segment,
        upload_single_file,
    },
    metrics::{
        log_compacted_segment_size_bytes,
        log_vector_prefetch_expiration,
        log_vector_prefetch_rejection,
        log_vectors_in_compacted_segment_total,
        vector_compact_construct_segment_seconds_timer,
        vector_compact_fetch_segments_seconds_timer,
        vector_compact_seconds_timer,
        vector_prefetch_timer,
    },
    SearchFileType,
};

#[derive(Clone)]
pub(crate) struct FragmentedSegmentFetcher<RT: Runtime> {
    archive_cache: ArchiveCacheManager<RT>,
    blocking_thread_pool: BoundedThreadPool<RT>,
}

pub struct FragmentedSegmentStorageKeys {
    pub segment: ObjectKey,
    pub id_tracker: ObjectKey,
    pub deleted_bitset: ObjectKey,
}

impl<RT: Runtime> FragmentedSegmentFetcher<RT> {
    /// blocking_thread_pool is used for small / fast IO operations and should
    /// be large.
    pub(crate) fn new(
        archive_cache: ArchiveCacheManager<RT>,
        blocking_thread_pool: BoundedThreadPool<RT>,
    ) -> FragmentedSegmentFetcher<RT> {
        Self {
            archive_cache,
            blocking_thread_pool,
        }
    }

    /// Fetch all parts of all fragmented segments with limited concurrency.
    pub fn stream_fetch_fragmented_segments<
        'a,
        T: TryInto<FragmentedSegmentStorageKeys> + Send + 'a,
    >(
        &'a self,
        search_storage: Arc<dyn Storage>,
        fragments: Vec<T>,
    ) -> impl Stream<Item = anyhow::Result<UntarredDiskSegmentPaths>> + '_
    where
        anyhow::Error: From<T::Error>,
    {
        stream::iter(fragments.into_iter().map(move |fragment| {
            self.fetch_fragmented_segment(search_storage.clone(), fragment).boxed()
        }))
        // Limit the parallel downloads a bit, we don't want to start and finish all downloads at
        // the same time. We want to be downloading and working with segments concurrently.
        .buffer_unordered(4)
    }

    /// Fetch all parts of an individual fragmented segment.
    pub async fn fetch_fragmented_segment<T: TryInto<FragmentedSegmentStorageKeys>>(
        &self,
        search_storage: Arc<dyn Storage>,
        fragment: T,
    ) -> anyhow::Result<UntarredDiskSegmentPaths>
    where
        anyhow::Error: From<T::Error>,
    {
        let paths: FragmentedSegmentStorageKeys = fragment.try_into()?;
        let archive_cache = self.archive_cache.clone();
        let segment_path = paths.segment.clone();
        let fetch_segment = archive_cache.get(
            search_storage.clone(),
            &segment_path,
            SearchFileType::FragmentedVectorSegment,
        );

        let fetch_id_tracker = self.fetch_single_file(
            search_storage.clone(),
            paths.id_tracker,
            SearchFileType::VectorIdTracker,
        );
        let fetch_bitset = self.fetch_single_file(
            search_storage.clone(),
            paths.deleted_bitset,
            SearchFileType::VectorDeletedBitset,
        );
        let (segment, id_tracker, bitset) =
            futures::try_join!(fetch_segment, fetch_id_tracker, fetch_bitset)?;
        Ok(UntarredDiskSegmentPaths::new(segment, id_tracker, bitset))
    }

    async fn fetch_single_file(
        &self,
        search_storage: Arc<dyn Storage>,
        storage_path: ObjectKey,
        file_type: SearchFileType,
    ) -> anyhow::Result<PathBuf> {
        // The archive cache always dumps things into directories, but we want a
        // specific file path.
        let parent_dir: PathBuf = self
            .archive_cache
            .get(search_storage, &storage_path, file_type)
            .await?;
        // tokio's async read_dir method punts to a thread pool too, but by using our
        // own, we can be runtime agnostic.
        let path = self
            .blocking_thread_pool
            .execute(move || try {
                let paths: Vec<_> = std::fs::read_dir(parent_dir)?
                    .map_ok(|value| value.path())
                    .try_collect()?;
                anyhow::ensure!(
                    paths.len() == 1,
                    "Expected one file but found multiple paths: {:?}",
                    paths,
                );
                paths.first().unwrap().to_owned()
            })
            .await??;
        Ok(path)
    }
}

pub(crate) struct FragmentedSegmentCompactor<RT: Runtime> {
    rt: RT,
    segment_fetcher: FragmentedSegmentFetcher<RT>,
    blocking_thread_pool: BoundedThreadPool<RT>,
}

impl<RT: Runtime> FragmentedSegmentCompactor<RT> {
    pub fn new(
        rt: RT,
        segment_fetcher: FragmentedSegmentFetcher<RT>,
        blocking_thread_pool: BoundedThreadPool<RT>,
    ) -> Self {
        Self {
            rt,
            segment_fetcher,
            blocking_thread_pool,
        }
    }

    pub async fn compact<'a, T: TryInto<FragmentedSegmentStorageKeys> + Send + 'a>(
        &'a self,
        segments: Vec<T>,
        dimension: usize,
        search_storage: Arc<dyn Storage>,
    ) -> anyhow::Result<FragmentedVectorSegment>
    where
        anyhow::Error: From<T::Error>,
        <T as TryInto<FragmentedSegmentStorageKeys>>::Error: From<std::io::Error> + Send,
        <T as TryInto<FragmentedSegmentStorageKeys>>::Error: From<anyhow::Error> + 'static,
    {
        let timer = vector_compact_seconds_timer();
        let fetch_timer = vector_compact_fetch_segments_seconds_timer();
        let segments: Vec<_> = self
            .segment_fetcher
            .stream_fetch_fragmented_segments(search_storage.clone(), segments)
            .and_then(|paths| async move {
                self.blocking_thread_pool
                    .execute(|| load_disk_segment(paths))
                    .await?
            })
            .try_collect()
            .await?;
        fetch_timer.finish();
        let total_segments = segments.len();

        let tmp_dir = TempDir::new()?;
        let scratch_dir = tmp_dir.path().join("scratch");
        let target_path = tmp_dir.path().join("segment");
        let new_segment = self
            .blocking_thread_pool
            .execute(move || {
                let timer = vector_compact_construct_segment_seconds_timer();
                std::fs::create_dir(&scratch_dir)?;
                std::fs::create_dir(&target_path)?;

                let result = merge_disk_segments_hnsw(
                    segments.iter().collect_vec(),
                    dimension,
                    &scratch_dir,
                    &target_path,
                )?;
                let segment_size = result.paths.segment.metadata()?.len();
                log_compacted_segment_size_bytes(segment_size);
                timer.finish();
                Ok(result)
            })
            .await??;

        let result = upload_segment(&self.rt, search_storage, new_segment).await?;
        // Ensure we own the temp dir through the entire upload
        drop(tmp_dir);
        tracing::debug!("Compacted {} segments", total_segments);
        timer.finish();
        log_vectors_in_compacted_segment_total(result.num_vectors);
        Ok(result)
    }
}

/// Fetches fragmented vector segments, allows their deleted bitsets to be
/// mutated, then re-uploads the deleted bitsets.
pub struct MutableFragmentedSegmentMetadata {
    // The original set of ObjectKeys that match the segment.
    original: FragmentedVectorSegment,
    // The loaded id tracker from the segment.
    id_tracker: VectorStaticIdTracker,
    // The loaded ldeleted bitset from the segment that may be modified with
    // additional deletes for the segment.
    mutated_deleted_bitset: DeletedBitset,
    // True if we've deleted at least one id, false otherwise.
    is_modified: bool,
}

impl MutableFragmentedSegmentMetadata {
    fn new(
        original: FragmentedVectorSegment,
        id_tracker: VectorStaticIdTracker,
        deleted_bitset: DeletedBitset,
    ) -> Self {
        Self {
            original,
            id_tracker,
            mutated_deleted_bitset: deleted_bitset,
            is_modified: false,
        }
    }

    pub async fn download(
        original: FragmentedVectorSegment,
        storage: Arc<dyn Storage>,
    ) -> anyhow::Result<Self> {
        // TODO(CX-5149): Improve the IO logic here.
        // Temp dir is fine because we're loading these into memory immediately.
        let tmp_dir = TempDir::new()?;
        let id_tracker_path = tmp_dir.path().join("id_tracker");
        download_single_file_zip(&original.id_tracker_key, &id_tracker_path, storage.clone())
            .await?;
        let deleted_bitset_path = tmp_dir.path().join("deleted_bitset");
        download_single_file_zip(&original.deleted_bitset_key, &deleted_bitset_path, storage)
            .await?;

        let deleted_bitset = DeletedBitset::load_from_path(deleted_bitset_path)?;

        // Clone is a bit of a hack here because these two deleted bitsets may become
        // inconsistent if one or more vectors are deleted via maybe_delete.
        // For now we don't care about the inconsistency because the loaded id tracker
        // is only used as part of maybe_delete, which is idempotent.
        let id_tracker = VectorStaticIdTracker(StaticIdTracker::load_from_path(
            id_tracker_path,
            deleted_bitset.clone(),
        )?);

        Ok(Self::new(original, id_tracker, deleted_bitset))
    }

    pub async fn upload_deleted_bitset(
        mut self,
        storage: Arc<dyn Storage>,
    ) -> anyhow::Result<FragmentedVectorSegment> {
        if !self.is_modified {
            return Ok(self.original);
        }

        let num_deleted = self.mutated_deleted_bitset.num_deleted() as u32;
        let mut buf = vec![];
        self.mutated_deleted_bitset.write(&mut buf)?;

        let filename = format!("deleted_bitset");
        let object_key = upload_single_file(
            &mut buf.as_slice(),
            filename,
            storage.clone(),
            SearchFileType::VectorDeletedBitset,
        )
        .await?;

        Ok(FragmentedVectorSegment {
            deleted_bitset_key: object_key,
            num_deleted,
            ..self.original
        })
    }

    pub fn maybe_delete(&mut self, external_id: ExtendedPointId) -> anyhow::Result<()> {
        if let Some(internal_id) = self.id_tracker.internal_id(external_id)
            // Documents may be updated N times, each of which will trigger a call to maybe_deleted.
            // We need to ignore deletes for already deleted points.
            // Check the mutated bitset in case the document was updated / deleted multiple times
            // in one round.
            && !self.mutated_deleted_bitset.is_deleted(internal_id)
        {
            self.mutated_deleted_bitset.delete(internal_id)?;
            self.is_modified = true;
        }
        Ok(())
    }
}

// A circular dependency workaround for search / database / vector.
impl PreviousSegment for MutableFragmentedSegmentMetadata {
    fn maybe_delete(&mut self, external_id: ExtendedPointId) -> anyhow::Result<()> {
        MutableFragmentedSegmentMetadata::maybe_delete(self, external_id)
    }
}

struct FragmentedSegmentPrefetchRequest {
    search_storage: Arc<dyn Storage>,
    fragments: Vec<FragmentedSegmentStorageKeys>,
}

pub(crate) struct FragmentedSegmentPrefetcher<RT: Runtime> {
    tx: CoDelQueueSender<RT, FragmentedSegmentPrefetchRequest>,
    _handle: <RT as Runtime>::Handle,
}

impl<RT: Runtime> FragmentedSegmentPrefetcher<RT> {
    pub(crate) fn new(
        rt: RT,
        fetcher: FragmentedSegmentFetcher<RT>,
        max_concurrent_fetches: usize,
    ) -> Self {
        let (tx, rx) =
            new_codel_queue_async::<_, FragmentedSegmentPrefetchRequest>(rt.clone(), 100);
        let handle = rt.spawn("prefetch_worker", async move {
            rx.filter_map(|(req, expired)| async move {
                if let Some(_expired) = expired {
                    log_vector_prefetch_expiration();
                    None
                } else {
                    Some(req)
                }
            })
            .map(
                |FragmentedSegmentPrefetchRequest {
                     search_storage,
                     fragments,
                 }| {
                    let fetcher = fetcher.clone();
                    async move {
                        for fragment in fragments {
                            let timer = vector_prefetch_timer();
                            fetcher
                                .fetch_fragmented_segment(search_storage.clone(), fragment)
                                .await?;
                            timer.finish();
                        }
                        Ok(())
                    }
                },
            )
            .buffer_unordered(max_concurrent_fetches)
            .for_each(|result: anyhow::Result<()>| async {
                if let Err(mut e) = result {
                    if e.downcast_ref::<ExpiredInQueue>().is_some()
                        || e.downcast_ref::<QueueFull>().is_some()
                    {
                        log_vector_prefetch_expiration();
                    } else {
                        report_error(&mut e);
                    }
                }
            })
            .await;
            tracing::info!("Prefetcher shutting down!")
        });
        Self {
            _handle: handle,
            tx,
        }
    }

    pub fn queue_prefetch(
        &self,
        search_storage: Arc<dyn Storage>,
        fragments: Vec<FragmentedSegmentStorageKeys>,
    ) -> anyhow::Result<()> {
        let result = self.tx.try_send(FragmentedSegmentPrefetchRequest {
            search_storage,
            fragments,
        });
        if result.is_err() {
            log_vector_prefetch_rejection();
        }
        Ok(())
    }
}
