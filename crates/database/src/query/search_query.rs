use async_trait::async_trait;
use common::{
    document::GenericDocument,
    index::IndexKeyBytes,
    knobs::TRANSACTION_MAX_READ_SIZE_BYTES,
    query::{
        CursorPosition,
        Search,
        SearchVersion,
    },
    runtime::Runtime,
    types::WriteTimestamp,
    version::{
        Version,
        MIN_NPM_VERSION_FOR_FUZZY_SEARCH,
    },
};
use errors::ErrorMetadata;
use search::{
    CandidateRevision,
    MAX_CANDIDATE_REVISIONS,
};
use value::TableIdentifier;

use super::{
    index_range::{
        soft_data_limit,
        CursorInterval,
    },
    IndexRangeResponse,
    QueryStream,
    QueryStreamNext,
    QueryType,
};
use crate::{
    metrics,
    Transaction,
};

/// A `QueryStream` that begins by querying a search index.
pub struct SearchQuery<T: QueryType> {
    query: Search,
    // Results are generated on the first call to SearchQuery::next.
    results: Option<SearchResultIterator<T>>,

    /// The interval defined by the optional start and end cursors.
    /// The start cursor will move as we produce results.
    cursor_interval: CursorInterval,
    version: Option<Version>,
}

impl<T: QueryType> SearchQuery<T> {
    pub fn new(query: Search, cursor_interval: CursorInterval, version: Option<Version>) -> Self {
        Self {
            query,
            results: None,
            cursor_interval,
            version,
        }
    }

    fn get_cli_gated_search_version(&self) -> SearchVersion {
        match &self.version {
            Some(v) if v >= &MIN_NPM_VERSION_FOR_FUZZY_SEARCH => SearchVersion::V2,
            _ => SearchVersion::V1,
        }
    }

    async fn search<RT: Runtime>(
        &self,
        tx: &mut Transaction<RT>,
    ) -> anyhow::Result<SearchResultIterator<T>> {
        let search_version = self.get_cli_gated_search_version();
        let revisions = tx.search(&self.query, search_version).await?;
        let revisions_in_range = revisions
            .into_iter()
            .filter(|(_, index_key)| self.cursor_interval.contains(index_key))
            .collect();
        let table_id = T::table_identifier(tx, &self.query.table)?;
        Ok(SearchResultIterator::new(
            revisions_in_range,
            table_id,
            self.version.clone(),
        ))
    }

    #[convex_macro::instrument_future]
    async fn _next<RT: Runtime>(
        &mut self,
        tx: &mut Transaction<RT>,
    ) -> anyhow::Result<Option<(GenericDocument<T::T>, WriteTimestamp)>> {
        let iterator = match &mut self.results {
            Some(results) => results,
            None => self.results.get_or_insert(self.search(tx).await?),
        };

        Ok(match iterator.next(tx).await? {
            None => {
                // We're out of results. If we have an end cursor then we must
                // have reached it. Otherwise we're at the end of the entire
                // query.
                self.cursor_interval.curr_exclusive = Some(
                    self.cursor_interval
                        .end_inclusive
                        .clone()
                        .unwrap_or(CursorPosition::End),
                );
                None
            },
            Some((next_document, next_index_key, next_timestamp)) => {
                self.cursor_interval.curr_exclusive = Some(CursorPosition::After(next_index_key));
                Some((next_document, next_timestamp))
            },
        })
    }
}

#[async_trait]
impl<T: QueryType> QueryStream<T> for SearchQuery<T> {
    fn cursor_position(&self) -> &Option<CursorPosition> {
        &self.cursor_interval.curr_exclusive
    }

    fn split_cursor_position(&self) -> Option<&CursorPosition> {
        // We could try to find a split cursor, but splitting a search query
        // doesn't make it more efficient, so for simplicity we can say splitting
        // isn't allowed.
        None
    }

    fn is_approaching_data_limit(&self) -> bool {
        self.results
            .as_ref()
            .map_or(false, |results| results.is_approaching_data_limit())
    }

    async fn next<RT: Runtime>(
        &mut self,
        tx: &mut Transaction<RT>,
        _prefetch_hint: Option<usize>,
    ) -> anyhow::Result<QueryStreamNext<T>> {
        self._next(tx).await.map(QueryStreamNext::Ready)
    }

    fn feed(&mut self, _index_range_response: IndexRangeResponse<T::T>) -> anyhow::Result<()> {
        anyhow::bail!("cannot feed an index range response into a search query");
    }
}

#[derive(Clone)]
struct SearchResultIterator<T: QueryType> {
    table_identifier: T::T,
    candidates: Vec<(CandidateRevision, IndexKeyBytes)>,
    next_index: usize,
    bytes_read: usize,
    version: Option<Version>,
}

impl<T: QueryType> SearchResultIterator<T> {
    fn new(
        candidates: Vec<(CandidateRevision, IndexKeyBytes)>,
        table_identifier: T::T,
        version: Option<Version>,
    ) -> Self {
        Self {
            table_identifier,
            candidates,
            next_index: 0,
            bytes_read: 0,
            version,
        }
    }

    fn is_approaching_data_limit(&self) -> bool {
        let soft_maximum_rows_read = soft_data_limit(MAX_CANDIDATE_REVISIONS);
        let soft_maximum_bytes_read = soft_data_limit(*TRANSACTION_MAX_READ_SIZE_BYTES);
        self.next_index > soft_maximum_rows_read || self.bytes_read > soft_maximum_bytes_read
    }

    async fn next<RT: Runtime>(
        &mut self,
        tx: &mut Transaction<RT>,
    ) -> anyhow::Result<Option<(GenericDocument<T::T>, IndexKeyBytes, WriteTimestamp)>> {
        let timer = metrics::search::iterator_next_timer();

        if self.next_index == MAX_CANDIDATE_REVISIONS {
            anyhow::bail!(ErrorMetadata::bad_request(
                "SearchQueryScannedTooManyDocumentsError",
                format!(
                    "Search query scanned too many documents (fetched {}). Consider using a \
                     smaller limit, paginating the query, or using a filter field to limit the \
                     number of documents pulled from the search index.",
                    MAX_CANDIDATE_REVISIONS
                )
            ))
        }

        let Some((candidate, index_key)) = self.candidates.get(self.next_index) else {
            timer.finish();
            return Ok(None);
        };

        self.next_index += 1;

        let id = self.table_identifier.clone().id(candidate.id);
        let (document, existing_doc_ts) = T::get_with_ts(tx, id.clone(), self.version.clone())
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("Unable to load search result {id}@{:?}", candidate.ts)
            })?;

        self.bytes_read += document.size();

        anyhow::ensure!(
            existing_doc_ts == candidate.ts,
            "Search result has incorrect timestamp. There's a bug in our search logic. id:{id} \
             existing_doc_ts:{existing_doc_ts:?} candidate_ts:{:?}",
            candidate.ts
        );

        timer.finish();
        Ok(Some((document, index_key.clone(), existing_doc_ts)))
    }
}
