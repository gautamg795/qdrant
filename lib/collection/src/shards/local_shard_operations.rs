use std::sync::Arc;

use async_trait::async_trait;
use futures::future::try_join_all;
use itertools::Itertools;
use segment::types::{
    ExtendedPointId, Filter, ScoredPoint, WithPayload, WithPayloadInterface, WithVector,
};
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use crate::collection_manager::segments_searcher::SegmentsSearcher;
use crate::common::stopping_guard::StoppingGuard;
use crate::operations::types::{
    CollectionError, CollectionInfo, CollectionResult, CoreSearchRequestBatch, CountRequest,
    CountResult, PointRequest, Record, SearchRequestBatch, UpdateResult, UpdateStatus,
};
use crate::operations::CollectionUpdateOperations;
use crate::optimizers_builder::DEFAULT_INDEXING_THRESHOLD_KB;
use crate::shards::local_shard::LocalShard;
use crate::shards::shard_trait::ShardOperation;
use crate::update_handler::{OperationData, UpdateSignal};

#[async_trait]
impl ShardOperation for LocalShard {
    /// Imply interior mutability.
    /// Performs update operation on this collection asynchronously.
    /// Explicitly waits for result to be updated.
    async fn update(
        &self,
        operation: CollectionUpdateOperations,
        wait: bool,
    ) -> CollectionResult<UpdateResult> {
        let (callback_sender, callback_receiver) = if wait {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        let operation_id = {
            let update_sender = self.update_sender.load();
            let channel_permit = update_sender.reserve().await?;
            let mut wal_lock = self.wal.lock();
            let operation_id = wal_lock.write(&operation)?;
            channel_permit.send(UpdateSignal::Operation(OperationData {
                op_num: operation_id,
                operation,
                sender: callback_sender,
                wait,
            }));
            operation_id
        };

        if let Some(receiver) = callback_receiver {
            let _res = receiver.await??;
            Ok(UpdateResult {
                operation_id,
                status: UpdateStatus::Completed,
            })
        } else {
            Ok(UpdateResult {
                operation_id,
                status: UpdateStatus::Acknowledged,
            })
        }
    }

    async fn scroll_by(
        &self,
        offset: Option<ExtendedPointId>,
        limit: usize,
        with_payload_interface: &WithPayloadInterface,
        with_vector: &WithVector,
        filter: Option<&Filter>,
        search_runtime_handle: &Handle,
    ) -> CollectionResult<Vec<Record>> {
        // ToDo: Make faster points selection with a set
        let segments = self.segments();
        let read_handles: Vec<_> = {
            let segments_guard = segments.read();
            segments_guard
                .iter()
                .map(|(_, segment)| {
                    let segment = segment.clone();
                    let filter = filter.cloned();
                    search_runtime_handle.spawn_blocking(move || {
                        segment
                            .get()
                            .read()
                            .read_filtered(offset, Some(limit), filter.as_ref())
                    })
                })
                .collect()
        };
        let all_points = try_join_all(read_handles).await?;

        let point_ids = all_points
            .into_iter()
            .flatten()
            .sorted()
            .dedup()
            .take(limit)
            .collect_vec();

        let with_payload = WithPayload::from(with_payload_interface);
        let mut points =
            SegmentsSearcher::retrieve(segments, &point_ids, &with_payload, with_vector)?;
        points.sort_by_key(|point| point.id);

        Ok(points)
    }

    /// Collect overview information about the shard
    async fn info(&self) -> CollectionResult<CollectionInfo> {
        Ok(self.local_shard_info().await)
    }

    async fn search(
        &self,
        request: Arc<SearchRequestBatch>,
        search_runtime_handle: &Handle,
    ) -> CollectionResult<Vec<Vec<ScoredPoint>>> {
        let (collection_params, indexing_threshold_kb) = {
            let collection_config = self.collection_config.read().await;
            (
                collection_config.params.clone(),
                collection_config
                    .optimizer_config
                    .indexing_threshold
                    .unwrap_or(DEFAULT_INDEXING_THRESHOLD_KB),
            )
        };

        let core_request = Arc::new(CoreSearchRequestBatch::from(request.as_ref().clone()));

        // check vector names existing
        for req in &core_request.searches {
            collection_params.get_vector_params(req.query.get_vector_name())?;
        }

        let is_stopped = StoppingGuard::new();

        let search_request = SegmentsSearcher::search(
            self.segments(),
            core_request.clone(),
            search_runtime_handle,
            true,
            is_stopped.get_is_stopped(),
            indexing_threshold_kb,
        );
        let timeout = self.shared_storage_config.search_timeout;
        let res: Vec<Vec<ScoredPoint>> = tokio::select! {
            res = search_request => res,
            _ = tokio::time::sleep(timeout) => {
                is_stopped.stop();
                log::debug!("Search timeout reached: {} seconds", timeout.as_secs());
                Err(CollectionError::timeout(timeout.as_secs() as usize, "Search"))
            }
        }?;

        let top_results = res
            .into_iter()
            .zip(core_request.searches.iter())
            .map(|(vector_res, req)| {
                let vector_name = req.query.get_vector_name();
                let distance = collection_params
                    .get_vector_params(vector_name)
                    .unwrap()
                    .distance;
                let processed_res = vector_res.into_iter().map(|mut scored_point| {
                    scored_point.score = distance.postprocess_score(scored_point.score);
                    scored_point
                });

                if let Some(threshold) = req.score_threshold {
                    processed_res
                        .take_while(|scored_point| {
                            distance.check_threshold(scored_point.score, threshold)
                        })
                        .collect()
                } else {
                    processed_res.collect()
                }
            })
            .collect();
        Ok(top_results)
    }

    async fn count(&self, request: Arc<CountRequest>) -> CollectionResult<CountResult> {
        let total_count = if request.exact {
            let all_points = self.read_filtered(request.filter.as_ref())?;
            all_points.len()
        } else {
            self.estimate_cardinality(request.filter.as_ref())?.exp
        };
        Ok(CountResult { count: total_count })
    }

    async fn retrieve(
        &self,
        request: Arc<PointRequest>,
        with_payload: &WithPayload,
        with_vector: &WithVector,
    ) -> CollectionResult<Vec<Record>> {
        SegmentsSearcher::retrieve(self.segments(), &request.ids, with_payload, with_vector)
    }
}
