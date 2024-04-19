use std::{
    collections::BTreeMap,
    ops::Bound,
    sync::Arc,
};

use anyhow::Context;
use bytes::Bytes;
use common::{
    runtime::{
        Runtime,
        UnixTimestamp,
    },
    sha256::Sha256Digest,
    types::ConvexOrigin,
};
use database::Transaction;
use errors::ErrorMetadata;
use futures::{
    stream::{
        self,
        BoxStream,
    },
    Stream,
    StreamExt,
};
use headers::{
    ContentLength,
    ContentRange,
    ContentType,
};
use keybroker::{
    Identity,
    KeyBroker,
};
use maplit::btreemap;
use mime::Mime;
use model::file_storage::{
    types::{
        FileStorageEntry,
        StorageUuid,
    },
    BatchKey,
    FileStorageId,
    FileStorageModel,
};
use storage::{
    Storage,
    StorageExt,
    Upload,
    UploadExt,
};
use usage_tracking::{
    StorageCallTracker,
    StorageUsageTracker,
};
use value::id_v6::DocumentIdV6;

use crate::{
    metrics::{
        self,
        log_get_file_chunk_size,
        GetFileType,
    },
    FileRangeStream,
    FileStorage,
    FileStream,
    TransactionalFileStorage,
};

const MAX_CHUNK_SIZE: usize = 32 * 1024;

impl<RT: Runtime> TransactionalFileStorage<RT> {
    pub fn new(rt: RT, storage: Arc<dyn Storage>, convex_origin: ConvexOrigin) -> Self {
        Self {
            rt,
            storage,
            convex_origin,
        }
    }

    pub fn generate_upload_url(
        &self,
        key_broker: &KeyBroker,
        issued_ts: UnixTimestamp,
    ) -> anyhow::Result<String> {
        let token = key_broker.issue_store_file_authorization(&self.rt, issued_ts)?;
        let origin = &self.convex_origin;

        Ok(format!("{origin}/api/storage/upload?token={token}"))
    }

    pub async fn get_url(
        &self,
        tx: &mut Transaction<RT>,
        storage_id: FileStorageId,
    ) -> anyhow::Result<Option<String>> {
        self.get_url_batch(tx, btreemap! { 0 => storage_id })
            .await
            .remove(&0)
            .context("batch_key missing")?
    }

    pub async fn get_url_batch(
        &self,
        tx: &mut Transaction<RT>,
        storage_ids: BTreeMap<BatchKey, FileStorageId>,
    ) -> BTreeMap<BatchKey, anyhow::Result<Option<String>>> {
        let origin = &self.convex_origin;
        let files = self.get_file_entry_batch(tx, storage_ids).await;
        files
            .into_iter()
            .map(|(batch_key, result)| {
                (
                    batch_key,
                    result.map(|file| {
                        file.map(|entry| format!("{origin}/api/storage/{}", entry.storage_id))
                    }),
                )
            })
            .collect()
    }

    pub async fn delete(
        &self,
        tx: &mut Transaction<RT>,
        storage_id: FileStorageId,
    ) -> anyhow::Result<()> {
        let success = self._delete(tx, storage_id.clone()).await?;
        if !success {
            anyhow::bail!(ErrorMetadata::not_found(
                "StorageIdNotFound",
                format!("storage id {storage_id} not found"),
            ));
        }
        Ok(())
    }

    pub async fn get_file_entry(
        &self,
        tx: &mut Transaction<RT>,
        storage_id: FileStorageId,
    ) -> anyhow::Result<Option<FileStorageEntry>> {
        self.get_file_entry_batch(tx, btreemap! { 0 => storage_id })
            .await
            .remove(&0)
            .context("batch_key missing")?
    }

    pub async fn get_file_entry_batch(
        &self,
        tx: &mut Transaction<RT>,
        storage_ids: BTreeMap<BatchKey, FileStorageId>,
    ) -> BTreeMap<BatchKey, anyhow::Result<Option<FileStorageEntry>>> {
        FileStorageModel::new(tx)
            .get_file_batch(storage_ids)
            .await
            .into_iter()
            .map(|(batch_key, result)| (batch_key, result.map(|r| r.map(|r| r.into_value()))))
            .collect()
    }

    pub async fn get_file_stream(
        &self,
        file: FileStorageEntry,
        usage_tracker: impl StorageUsageTracker + Clone + 'static,
    ) -> anyhow::Result<FileStream> {
        let sha256 = file.sha256.clone();

        let result = self
            .file_stream(
                file,
                (Bound::Included(0), Bound::Unbounded),
                usage_tracker,
                GetFileType::All,
            )
            .await?;

        Ok(FileStream {
            sha256,
            content_length: result.content_length,
            content_type: result.content_type,
            stream: result.stream,
        })
    }

    pub async fn get_file_range_stream(
        &self,
        file: FileStorageEntry,
        bytes_range: (Bound<u64>, Bound<u64>),
        usage_tracker: impl StorageUsageTracker + Clone + 'static,
    ) -> anyhow::Result<FileRangeStream> {
        self.file_stream(file, bytes_range, usage_tracker, GetFileType::Range)
            .await
    }

    async fn file_stream(
        &self,
        file: FileStorageEntry,
        bytes_range: (Bound<u64>, Bound<u64>),
        usage_tracker: impl StorageUsageTracker + Clone + 'static,
        get_file_type: GetFileType,
    ) -> anyhow::Result<FileRangeStream> {
        let FileStorageEntry {
            storage_id: _,
            storage_key,
            sha256: _,
            size,
            content_type,
        } = file;

        let content_type = match content_type {
            None => None,
            Some(ct) => Some(ct.parse::<Mime>()?.into()),
        };

        let storage_get_stream = self
            .storage
            .get_range(&storage_key.to_string().try_into()?, bytes_range)
            .await?
            .with_context(|| format!("object {storage_key:?} not found"))?;
        let content_range = ContentRange::bytes(bytes_range, size as u64)?;
        let stream = storage_get_stream.stream;
        let content_length = ContentLength(storage_get_stream.content_length as u64);

        let call_tracker = usage_tracker.track_storage_call("get range");

        Ok(FileRangeStream {
            content_length,
            content_range,
            content_type,
            stream: Self::track_stream_usage(stream, get_file_type, call_tracker),
        })
    }

    fn track_stream_usage(
        stream: BoxStream<'static, futures::io::Result<bytes::Bytes>>,
        get_file_type: GetFileType,
        storage_call_tracker: Box<dyn StorageCallTracker>,
    ) -> BoxStream<'static, futures::io::Result<bytes::Bytes>> {
        Box::pin(
            stream
                .flat_map(|bytes| {
                    // The input chunk size here depends on the Storage implementation. Our upstream
                    // provider seems to send chunks between 1kb and 16kb. Our
                    // file storage will send entire files (80+MB).
                    // The chunk size here determines the maximum amount we will round up a
                    // customer if they read a single byte. The larger our chunk size, the more we
                    // round for that byte. So we set a maximum chunk size to limit the maximum
                    // amount we round up if the upstream provider sends us a large chunk.
                    stream::iter(if let Ok(bytes) = bytes {
                        if bytes.len() <= MAX_CHUNK_SIZE {
                            vec![Ok(bytes)]
                        } else {
                            bytes
                                .chunks(MAX_CHUNK_SIZE)
                                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                                .collect::<Vec<_>>()
                        }
                    } else {
                        vec![bytes]
                    })
                })
                .map(move |bytes: futures::io::Result<bytes::Bytes>| {
                    if let Ok(ref bytes) = bytes {
                        let bytes_size = bytes.len() as u64;
                        log_get_file_chunk_size(bytes_size, get_file_type);
                        storage_call_tracker.track_storage_egress_size(bytes_size);
                    }
                    bytes
                }),
        )
    }

    async fn _delete(
        &self,
        tx: &mut Transaction<RT>,
        storage_id: FileStorageId,
    ) -> anyhow::Result<bool> {
        let did_delete = FileStorageModel::new(tx)
            .delete_file(storage_id, Identity::system())
            .await?
            .is_some();
        Ok(did_delete)
    }

    /// `upload_file` just uploads a file to storage. It does not save the file
    /// in the _file_storage system table and it does not count towards
    /// usage. The caller is responsible to call `store_file_entry` to
    /// actually persist the entry and manually account for usage.
    pub async fn upload_file(
        &self,
        content_length: Option<ContentLength>,
        content_type: Option<ContentType>,
        file: impl Stream<Item = anyhow::Result<impl Into<Bytes>>> + Send,
        expected_sha256: Option<Sha256Digest>,
    ) -> anyhow::Result<FileStorageEntry> {
        let storage_id = StorageUuid::from(self.rt.new_uuid_v4());

        tracing::info!("Uploading with content length {content_length:?}");
        let timer = metrics::store_file_timer();

        let mut upload = self.storage.start_upload().await?;
        let file = file.map(|chunk| chunk.map(|chunk| chunk.into()));
        let (size, actual_sha256) = upload.try_write_parallel_and_hash(file).await?;
        if let Some(expected_sha256) = expected_sha256
            && expected_sha256 != actual_sha256
        {
            let msg = format!(
                "Sha256 mismatch. Expected: {} Actual: {}",
                expected_sha256.as_base64(),
                actual_sha256.as_base64()
            );

            anyhow::bail!(ErrorMetadata::bad_request("Sha256Mismatch", msg));
        }

        // Key in underlying storage is a different UUID from the one we hand out.
        let storage_key = upload.complete().await?;

        let elapsed = timer.finish();
        tracing::info!(
            "Wrote file {size} to {storage_key:?}. Total:{elapsed:?} ContentType:{content_type:?}",
        );

        let entry = FileStorageEntry {
            storage_id,
            storage_key,
            sha256: actual_sha256,
            size: size.try_into()?,
            content_type: content_type.map(|ct| ct.to_string()),
        };

        Ok(entry)
    }

    /// Stores a file entry generated by upload_file(). The caller is
    /// responsible to track usage. If you are outside of the
    /// isolate environment, it is recommended to use FileStorage::store_file
    /// that performs all necessary steps instead.
    pub async fn store_file_entry(
        &self,
        tx: &mut Transaction<RT>,
        entry: FileStorageEntry,
    ) -> anyhow::Result<DocumentIdV6> {
        let table_mapping = tx.table_mapping().clone();
        let system_doc_id = FileStorageModel::new(tx).store_file(entry).await?;
        let virtual_id = tx
            .virtual_system_mapping()
            .system_resolved_id_to_virtual_developer_id(
                &system_doc_id,
                &table_mapping,
                &tx.virtual_table_mapping().clone(),
            )?;

        Ok(virtual_id)
    }
}

impl<RT: Runtime> FileStorage<RT> {
    pub async fn store_file(
        &self,
        content_length: Option<ContentLength>,
        content_type: Option<ContentType>,
        file: impl Stream<Item = anyhow::Result<impl Into<Bytes>>> + Send,
        expected_sha256: Option<Sha256Digest>,
        usage_tracker: &dyn StorageUsageTracker,
    ) -> anyhow::Result<DocumentIdV6> {
        let entry = self
            .transactional_file_storage
            .upload_file(content_length, content_type, file, expected_sha256)
            .await?;
        let size = entry.size;

        // Start/Complete transaction after the slow upload process
        // to avoid OCC risk.
        let mut tx = self.database.begin(Identity::system()).await?;
        let virtual_id = self
            .transactional_file_storage
            .store_file_entry(&mut tx, entry)
            .await?;
        self.database
            .commit_with_write_source(tx, "file_storage_store_file")
            .await?;

        usage_tracker
            .track_storage_call("store")
            .track_storage_ingress_size(size as u64);
        Ok(virtual_id)
    }
}
