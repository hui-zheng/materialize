// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Logic for processing [`Coordinator`] messages. The [`Coordinator`] receives
//! messages from various sources (ex: controller, clients, background tasks, etc).

use std::collections::{btree_map, BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use futures::future::LocalBoxFuture;
use futures::FutureExt;
use maplit::btreemap;
use mz_catalog::memory::objects::ClusterReplicaProcessStatus;
use mz_controller::clusters::{ClusterEvent, ClusterStatus};
use mz_controller::ControllerResponse;
use mz_ore::now::EpochMillis;
use mz_ore::option::OptionExt;
use mz_ore::tracing::OpenTelemetryContext;
use mz_ore::{soft_assert_or_log, task};
use mz_persist_client::usage::ShardsUsageReferenced;
use mz_sql::ast::Statement;
use mz_sql::names::ResolvedIds;
use mz_sql::pure::PurifiedStatement;
use mz_storage_types::controller::CollectionMetadata;
use opentelemetry::trace::TraceContextExt;
use rand::{rngs, Rng, SeedableRng};
use serde_json::json;
use tracing::{event, info_span, warn, Instrument, Level};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::active_compute_sink::{ActiveComputeSink, ActiveComputeSinkRetireReason};
use crate::command::Command;
use crate::coord::appends::Deferred;
use crate::coord::{
    AlterConnectionValidationReady, ClusterReplicaStatuses, Coordinator,
    CreateConnectionValidationReady, Message, PurifiedStatementReady, WatchSetResponse,
};
use crate::telemetry::{EventDetails, SegmentClientExt};
use crate::{catalog, AdapterNotice, TimestampContext};

impl Coordinator {
    /// BOXED FUTURE: As of Nov 2023 the returned Future from this function was 74KB. This would
    /// get stored on the stack which is bad for runtime performance, and blow up our stack usage.
    /// Because of that we purposefully move this Future onto the heap (i.e. Box it).
    ///
    /// We pass in a span from the outside, rather than instrumenting this
    /// method using `#instrument[...]` or calling `.instrument()` at the
    /// callsite so that we can correctly instrument the boxed future here _and_
    /// so that we can stitch up the OpenTelemetryContext when we're processing
    /// a `Message::Command` or other commands that pass around a context.
    pub(crate) fn handle_message<'a>(
        &'a mut self,
        span: tracing::Span,
        msg: Message,
    ) -> LocalBoxFuture<'a, ()> {
        async move {
            match msg {
                Message::Command(otel_ctx, cmd) => {
                    // TODO: We need a Span that is not none for the otel_ctx to attach the parent
                    // relationship to. If we swap the otel_ctx in `Command::Message` for a Span, we
                    // can downgrade this to a debug_span.
                    let span = tracing::info_span!("message_command").or_current();
                    span.in_scope(|| otel_ctx.attach_as_parent());
                    self.message_command(cmd).instrument(span).await
                }
                Message::ControllerReady => {
                    let Coordinator {
                        controller,
                        catalog,
                        ..
                    } = self;
                    let storage_metadata = catalog.state().storage_metadata();
                    if let Some(m) = controller
                        .process(storage_metadata)
                        .await
                        .expect("`process` never returns an error")
                    {
                        self.message_controller(m).await
                    }
                }
                Message::PurifiedStatementReady(ready) => {
                    self.message_purified_statement_ready(ready).await
                }
                Message::CreateConnectionValidationReady(ready) => {
                    self.message_create_connection_validation_ready(ready).await
                }
                Message::AlterConnectionValidationReady(ready) => {
                    self.message_alter_connection_validation_ready(ready).await
                }
                Message::WriteLockGrant(write_lock_guard) => {
                    self.message_write_lock_grant(write_lock_guard).await;
                }
                Message::GroupCommitInitiate(span, permit) => {
                    // Add an OpenTelemetry link to our current span.
                    tracing::Span::current().add_link(span.context().span().span_context().clone());
                    self.try_group_commit(permit).instrument(span).await
                }
                Message::GroupCommitApply(timestamp, responses, write_lock_guard, permit) => {
                    self.group_commit_apply(timestamp, responses, write_lock_guard, permit)
                        .await;
                }
                Message::AdvanceTimelines => {
                    self.advance_timelines().await;
                }
                Message::DropReadHolds(dropped_read_holds) => {
                    tracing::debug!(?dropped_read_holds, "releasing dropped read holds!");
                    self.release_read_holds(dropped_read_holds);
                }
                Message::ClusterEvent(event) => self.message_cluster_event(event).await,
                Message::CancelPendingPeeks { conn_id } => {
                    self.cancel_pending_peeks(&conn_id);
                }
                Message::LinearizeReads => {
                    self.message_linearize_reads().await;
                }
                Message::StorageUsageSchedule => {
                    self.schedule_storage_usage_collection().await;
                }
                Message::StorageUsageFetch => {
                    self.storage_usage_fetch().await;
                }
                Message::StorageUsageUpdate(sizes) => {
                    self.storage_usage_update(sizes).await;
                }
                Message::RetireExecute {
                    otel_ctx,
                    data,
                    reason,
                } => {
                    otel_ctx.attach_as_parent();
                    self.retire_execution(reason, data);
                }
                Message::ExecuteSingleStatementTransaction {
                    ctx,
                    otel_ctx,
                    stmt,
                    params,
                } => {
                    otel_ctx.attach_as_parent();
                    self.sequence_execute_single_statement_transaction(ctx, stmt, params)
                        .await;
                }
                Message::PeekStageReady {
                    ctx,
                    span,
                    stage,
                } => {
                    self.sequence_staged(ctx, span, stage).await;
                }
                Message::CreateIndexStageReady {
                    ctx,
                    span,
                    stage,
                } => {
                    self.sequence_staged(ctx, span, stage).await;
                }
                Message::CreateViewStageReady {
                    ctx,
                    span,
                    stage,
                } => {
                    self.sequence_staged(ctx, span, stage).await;
                }
                Message::CreateMaterializedViewStageReady {
                    ctx,
                    span,
                    stage,
                } => {
                    self.sequence_staged(ctx, span, stage).await;
                }
                Message::SubscribeStageReady {
                    ctx,
                    span,
                    stage,
                } => {
                    self.sequence_staged(ctx, span, stage).await;
                }
                Message::IntrospectionSubscribeStageReady {
                    span,
                    stage,
                } => {
                    self.sequence_staged((), span, stage).await;
                }
                Message::ExplainTimestampStageReady {
                    ctx,
                    span,
                    stage,
                } => {
                    self.sequence_staged(ctx, span, stage).await;
                }
                Message::SecretStageReady {
                    ctx,
                    span,
                    stage,
                } => {
                    self.sequence_staged(ctx, span, stage).await;
                }
                Message::ClusterStageReady{
                    ctx,
                    span,
                    stage,
                } => {
                    self.sequence_staged(ctx, span, stage).await;
                }
                Message::DrainStatementLog => {
                    self.drain_statement_log().await;
                }
                Message::PrivateLinkVpcEndpointEvents(events) => {
                    if !self.controller.read_only() {
                        self.controller
                            .storage
                            .append_introspection_updates(
                                mz_storage_client::controller::IntrospectionType::PrivatelinkConnectionStatusHistory,
                                events
                                    .into_iter()
                                    .map(|e| (mz_repr::Row::from(e), 1))
                                    .collect(),
                            )
                            .await;
                    }
                }
                Message::CheckSchedulingPolicies => {
                    self.check_scheduling_policies().await;
                }
                Message::SchedulingDecisions(decisions) => {
                    self.handle_scheduling_decisions(decisions).await;
                }
                Message::DeferredStatementReady => {
                    self.handle_deferred_statement().await;
                }
            }
        }
        .instrument(span)
        .boxed_local()
    }

    #[mz_ore::instrument(level = "debug")]
    pub async fn storage_usage_fetch(&mut self) {
        let internal_cmd_tx = self.internal_cmd_tx.clone();
        let client = self.storage_usage_client.clone();

        // Record the currently live shards.
        let live_shards: BTreeSet<_> = self
            .controller
            .storage
            .active_collection_metadatas()
            .into_iter()
            .flat_map(|(_id, collection_metadata)| {
                let CollectionMetadata {
                    data_shard,
                    remap_shard,
                    status_shard,
                    // No wildcards, to improve the odds that the addition of a
                    // new shard type results in a compiler error here.
                    //
                    // ATTENTION: If you add a new type of shard that is
                    // associated with a collection, almost surely you should
                    // return it below, so that its usage is recorded in the
                    // `mz_storage_usage_by_shard` table.
                    persist_location: _,
                    relation_desc: _,
                    txns_shard: _,
                } = collection_metadata;
                [remap_shard, status_shard, Some(data_shard)].into_iter()
            })
            .filter_map(|shard| shard)
            .collect();

        let collection_metric = self
            .metrics
            .storage_usage_collection_time_seconds
            .with_label_values(&[]);

        // Spawn an asynchronous task to compute the storage usage, which
        // requires a slow scan of the underlying storage engine.
        task::spawn(|| "storage_usage_fetch", async move {
            let collection_metric_timer = collection_metric.start_timer();
            let shard_sizes = client.shards_usage_referenced(live_shards).await;
            collection_metric_timer.observe_duration();

            // It is not an error for shard sizes to become ready after
            // `internal_cmd_rx` is dropped.
            if let Err(e) = internal_cmd_tx.send(Message::StorageUsageUpdate(shard_sizes)) {
                warn!("internal_cmd_rx dropped before we could send: {:?}", e);
            }
        });
    }

    #[mz_ore::instrument(level = "debug")]
    async fn storage_usage_update(&mut self, shards_usage: ShardsUsageReferenced) {
        // Similar to audit events, use the oracle ts so this is guaranteed to
        // increase. This is intentionally the timestamp of when collection
        // finished, not when it started, so that we don't write data with a
        // timestamp in the past.
        let collection_timestamp: EpochMillis = self.get_local_write_ts().await.timestamp.into();

        let mut ops = vec![];
        for (shard_id, shard_usage) in shards_usage.by_shard {
            ops.push(catalog::Op::UpdateStorageUsage {
                shard_id: Some(shard_id.to_string()),
                size_bytes: shard_usage.size_bytes(),
                collection_timestamp,
            });
        }

        match self.catalog_transact_inner(None, ops).await {
            Ok(table_updates) => {
                let internal_cmd_tx = self.internal_cmd_tx.clone();
                let mut task_span =
                    info_span!(parent: None, "coord::storage_usage_update::table_updates");
                OpenTelemetryContext::obtain().attach_as_parent_to(&mut task_span);
                task::spawn(|| "storage_usage_update_table_updates", async move {
                    table_updates.instrument(task_span).await;
                    // It is not an error for this task to be running after `internal_cmd_rx` is dropped.
                    if let Err(e) = internal_cmd_tx.send(Message::StorageUsageSchedule) {
                        warn!("internal_cmd_rx dropped before we could send: {e:?}");
                    }
                });
            }
            Err(err) => tracing::warn!("Failed to update storage metrics: {:?}", err),
        }
    }

    pub async fn schedule_storage_usage_collection(&self) {
        // Instead of using an `tokio::timer::Interval`, we calculate the time until the next
        // usage collection and wait for that amount of time. This is so we can keep the intervals
        // consistent even across restarts. If collection takes too long, it is possible that
        // we miss an interval.

        // 1) Deterministically pick some offset within the collection interval to prevent
        // thundering herds across environments.
        const SEED_LEN: usize = 32;
        let mut seed = [0; SEED_LEN];
        for (i, byte) in self
            .catalog()
            .state()
            .config()
            .environment_id
            .organization_id()
            .as_bytes()
            .into_iter()
            .take(SEED_LEN)
            .enumerate()
        {
            seed[i] = *byte;
        }
        let storage_usage_collection_interval_ms: EpochMillis =
            EpochMillis::try_from(self.storage_usage_collection_interval.as_millis())
                .expect("storage usage collection interval must fit into u64");
        let offset =
            rngs::SmallRng::from_seed(seed).gen_range(0..storage_usage_collection_interval_ms);
        let now_ts: EpochMillis = self.peek_local_write_ts().await.into();

        // 2) Determine the amount of ms between now and the next collection time.
        let previous_collection_ts =
            (now_ts - (now_ts % storage_usage_collection_interval_ms)) + offset;
        let next_collection_ts = if previous_collection_ts > now_ts {
            previous_collection_ts
        } else {
            previous_collection_ts + storage_usage_collection_interval_ms
        };
        let next_collection_interval = Duration::from_millis(next_collection_ts - now_ts);

        // 3) Sleep for that amount of time, then initiate another storage usage collection.
        let internal_cmd_tx = self.internal_cmd_tx.clone();
        task::spawn(|| "storage_usage_collection", async move {
            tokio::time::sleep(next_collection_interval).await;
            if internal_cmd_tx.send(Message::StorageUsageFetch).is_err() {
                // If sending fails, the main thread has shutdown.
            }
        });
    }

    #[mz_ore::instrument(level = "debug")]
    async fn message_command(&mut self, cmd: Command) {
        self.handle_command(cmd).await;
    }

    #[mz_ore::instrument(level = "debug")]
    async fn message_controller(&mut self, message: ControllerResponse) {
        event!(Level::TRACE, message = format!("{:?}", message));
        match message {
            ControllerResponse::PeekResponse(uuid, response, otel_ctx) => {
                self.send_peek_response(uuid, response, otel_ctx);
            }
            ControllerResponse::SubscribeResponse(sink_id, response) => {
                if let Some(ActiveComputeSink::Subscribe(active_subscribe)) =
                    self.active_compute_sinks.get_mut(&sink_id)
                {
                    let finished = active_subscribe.process_response(response);
                    if finished {
                        self.retire_compute_sinks(btreemap! {
                            sink_id => ActiveComputeSinkRetireReason::Finished,
                        })
                        .await;
                    }

                    soft_assert_or_log!(
                        !self.introspection_subscribes.contains_key(&sink_id),
                        "`sink_id` {sink_id} unexpectedly found in both `active_subscribes` \
                         and `introspection_subscribes`",
                    );
                } else if self.introspection_subscribes.contains_key(&sink_id) {
                    self.handle_introspection_subscribe_batch(sink_id, response)
                        .await;
                } else {
                    tracing::error!(%sink_id, "received SubscribeResponse for nonexistent subscribe");
                }
            }
            ControllerResponse::CopyToResponse(sink_id, response) => {
                match self.drop_compute_sink(sink_id).await {
                    Some(ActiveComputeSink::CopyTo(active_copy_to)) => {
                        active_copy_to.retire_with_response(response);
                    }
                    _ => {
                        tracing::error!(%sink_id, "received CopyToResponse for nonexistent copy to");
                    }
                }
            }
            ControllerResponse::ComputeReplicaMetrics(replica_id, new) => {
                let m = match self
                    .transient_replica_metadata
                    .entry(replica_id)
                    .or_insert_with(|| Some(Default::default()))
                {
                    // `None` is the tombstone for a removed replica
                    None => return,
                    Some(md) => &mut md.metrics,
                };
                let old = std::mem::replace(m, Some(new.clone()));
                if old.as_ref() != Some(&new) {
                    let retractions = old.map(|old| {
                        self.catalog()
                            .state()
                            .pack_replica_metric_updates(replica_id, &old, -1)
                    });
                    let insertions = self
                        .catalog()
                        .state()
                        .pack_replica_metric_updates(replica_id, &new, 1);
                    let updates = if let Some(retractions) = retractions {
                        retractions
                            .into_iter()
                            .chain(insertions.into_iter())
                            .collect()
                    } else {
                        insertions
                    };
                    let updates = self
                        .catalog()
                        .state()
                        .resolve_builtin_table_updates(updates);
                    self.builtin_table_update().background(updates);
                }
            }
            ControllerResponse::WatchSetFinished(ws_ids) => {
                let now = self.now();
                for ws_id in ws_ids {
                    let Some((conn_id, rsp)) = self.installed_watch_sets.remove(&ws_id) else {
                        continue;
                    };
                    self.connection_watch_sets
                        .get_mut(&conn_id)
                        .expect("corrupted coordinator state: unknown connection id")
                        .remove(&ws_id);
                    if self.connection_watch_sets[&conn_id].is_empty() {
                        self.connection_watch_sets.remove(&conn_id);
                    }

                    match rsp {
                        WatchSetResponse::StatementDependenciesReady(id, ev) => {
                            self.record_statement_lifecycle_event(&id, &ev, now);
                        }
                        WatchSetResponse::AlterSinkReady(ctx) => {
                            self.sequence_alter_sink_finish(ctx).await;
                        }
                    }
                }
            }
        }
    }

    #[mz_ore::instrument(level = "debug")]
    async fn message_purified_statement_ready(
        &mut self,
        PurifiedStatementReady {
            ctx,
            result,
            params,
            mut plan_validity,
            original_stmt,
            otel_ctx,
        }: PurifiedStatementReady,
    ) {
        otel_ctx.attach_as_parent();

        // Ensure that all dependencies still exist after purification, as a
        // `DROP CONNECTION` or other `DROP` may have sneaked in. If any have gone missing, we
        // repurify the original statement. This will either produce a nice
        // "unknown connector" error, or pick up a new connector that has
        // replaced the dropped connector.
        //
        // n.b. an `ALTER CONNECTION` occurring during purification is OK
        // because we always look up/populate a connection's state after
        // committing to the catalog, so are guaranteed to see the connection's
        // most recent version.
        if plan_validity.check(self.catalog()).is_err() {
            self.handle_execute_inner(original_stmt, params, ctx).await;
            return;
        }

        let purified_statement = match result {
            Ok(ok) => ok,
            Err(e) => return ctx.retire(Err(e)),
        };

        let plan = match purified_statement {
            PurifiedStatement::PurifiedCreateSource {
                create_progress_subsource_stmt,
                create_source_stmt,
                subsources,
            } => {
                self.plan_purified_create_source(
                    &ctx,
                    params,
                    create_progress_subsource_stmt,
                    create_source_stmt,
                    subsources,
                )
                .await
            }
            PurifiedStatement::PurifiedAlterSourceAddSubsources {
                source_name,
                options,
                subsources,
            } => {
                self.plan_purified_alter_source_add_subsource(
                    ctx.session(),
                    params,
                    source_name,
                    options,
                    subsources,
                )
                .await
            }
            o @ (PurifiedStatement::PurifiedAlterSource { .. }
            | PurifiedStatement::PurifiedCreateSink(..)) => {
                // Unify these into a `Statement`.
                let stmt = match o {
                    PurifiedStatement::PurifiedAlterSource { alter_source_stmt } => {
                        Statement::AlterSource(alter_source_stmt)
                    }
                    PurifiedStatement::PurifiedCreateSink(stmt) => Statement::CreateSink(stmt),
                    PurifiedStatement::PurifiedCreateSource { .. }
                    | PurifiedStatement::PurifiedAlterSourceAddSubsources { .. } => {
                        unreachable!("not part of exterior match stmt")
                    }
                };

                // Determine all dependencies, not just those in the statement
                // itself.
                let resolved_ids = mz_sql::names::visit_dependencies(&stmt);
                self.plan_statement(ctx.session(), stmt, &params, &resolved_ids)
                    .map(|plan| (plan, resolved_ids))
            }
        };

        match plan {
            Ok((plan, resolved_ids)) => self.sequence_plan(ctx, plan, resolved_ids).await,
            Err(e) => ctx.retire(Err(e)),
        }
    }

    #[mz_ore::instrument(level = "debug")]
    async fn message_create_connection_validation_ready(
        &mut self,
        CreateConnectionValidationReady {
            mut ctx,
            result,
            connection_gid,
            mut plan_validity,
            otel_ctx,
            dependency_ids,
        }: CreateConnectionValidationReady,
    ) {
        otel_ctx.attach_as_parent();

        // Ensure that all dependencies still exist after validation, as a
        // `DROP SECRET` may have sneaked in.
        //
        // WARNING: If we support `ALTER SECRET`, we'll need to also check
        // for connectors that were altered while we were purifying.
        if let Err(e) = plan_validity.check(self.catalog()) {
            let _ = self.secrets_controller.delete(connection_gid).await;
            return ctx.retire(Err(e));
        }

        let plan = match result {
            Ok(ok) => ok,
            Err(e) => {
                let _ = self.secrets_controller.delete(connection_gid).await;
                return ctx.retire(Err(e));
            }
        };

        let result = self
            .sequence_create_connection_stage_finish(
                ctx.session_mut(),
                connection_gid,
                plan,
                ResolvedIds(dependency_ids),
            )
            .await;
        ctx.retire(result);
    }

    #[mz_ore::instrument(level = "debug")]
    async fn message_alter_connection_validation_ready(
        &mut self,
        AlterConnectionValidationReady {
            mut ctx,
            result,
            connection_gid,
            mut plan_validity,
            otel_ctx,
            dependency_ids: _,
        }: AlterConnectionValidationReady,
    ) {
        otel_ctx.attach_as_parent();

        // Ensure that all dependencies still exist after validation, as a
        // `DROP SECRET` may have sneaked in.
        //
        // WARNING: If we support `ALTER SECRET`, we'll need to also check
        // for connectors that were altered while we were purifying.
        if let Err(e) = plan_validity.check(self.catalog()) {
            return ctx.retire(Err(e));
        }

        let conn = match result {
            Ok(ok) => ok,
            Err(e) => {
                return ctx.retire(Err(e));
            }
        };

        let result = self
            .sequence_alter_connection_stage_finish(ctx.session_mut(), connection_gid, conn)
            .await;
        ctx.retire(result);
    }

    #[mz_ore::instrument(level = "debug")]
    async fn message_write_lock_grant(
        &mut self,
        write_lock_guard: tokio::sync::OwnedMutexGuard<()>,
    ) {
        // It's possible to have more incoming write lock grants
        // than pending writes because of cancellations.
        if let Some(ready) = self.write_lock_wait_group.pop_front() {
            match ready {
                Deferred::Plan(mut ready) => {
                    ready.ctx.session_mut().grant_write_lock(write_lock_guard);
                    if let Err(e) = ready.validity.check(self.catalog()) {
                        ready.ctx.retire(Err(e))
                    } else {
                        // Write statements never need to track resolved IDs (NOTE: This is not the
                        // same thing as plan dependencies, which we do need to re-validate).
                        let resolved_ids = ResolvedIds(BTreeSet::new());
                        self.sequence_plan(ready.ctx, ready.plan, resolved_ids)
                            .await;
                    }
                }
                Deferred::GroupCommit => {
                    self.group_commit_initiate(Some(write_lock_guard), None)
                        .await
                }
            }
        }
        // N.B. if no deferred plans, write lock is released by drop
        // here.
    }

    #[mz_ore::instrument(level = "debug")]
    async fn message_cluster_event(&mut self, event: ClusterEvent) {
        event!(Level::TRACE, event = format!("{:?}", event));

        if let Some(segment_client) = &self.segment_client {
            let env_id = &self.catalog().config().environment_id;
            let mut properties = json!({
                "cluster_id": event.cluster_id.to_string(),
                "replica_id": event.replica_id.to_string(),
                "process_id": event.process_id,
                "status": event.status.as_kebab_case_str(),
            });
            match event.status {
                ClusterStatus::Ready => (),
                ClusterStatus::NotReady(reason) => {
                    let properties = match &mut properties {
                        serde_json::Value::Object(map) => map,
                        _ => unreachable!(),
                    };
                    properties.insert(
                        "reason".into(),
                        json!(reason.display_or("unknown").to_string()),
                    );
                }
            };
            segment_client.environment_track(
                env_id,
                "Cluster Changed Status",
                properties,
                EventDetails {
                    timestamp: Some(event.time),
                    ..Default::default()
                },
            );
        }

        // It is possible that we receive a status update for a replica that has
        // already been dropped from the catalog. Just ignore these events.
        let Some(replica_statues) = self
            .cluster_replica_statuses
            .try_get_cluster_replica_statuses(event.cluster_id, event.replica_id)
        else {
            return;
        };

        if event.status != replica_statues[&event.process_id].status {
            let old_replica_status =
                ClusterReplicaStatuses::cluster_replica_status(replica_statues);
            let old_process_status = replica_statues
                .get(&event.process_id)
                .expect("Process exists");
            let builtin_table_retraction =
                self.catalog().state().pack_cluster_replica_status_update(
                    event.replica_id,
                    event.process_id,
                    old_process_status,
                    -1,
                );
            let builtin_table_retraction = self
                .catalog()
                .state()
                .resolve_builtin_table_update(builtin_table_retraction);

            let new_process_status = ClusterReplicaProcessStatus {
                status: event.status,
                time: event.time,
            };
            let builtin_table_addition = self.catalog().state().pack_cluster_replica_status_update(
                event.replica_id,
                event.process_id,
                &new_process_status,
                1,
            );
            let builtin_table_addition = self
                .catalog()
                .state()
                .resolve_builtin_table_update(builtin_table_addition);
            self.cluster_replica_statuses.ensure_cluster_status(
                event.cluster_id,
                event.replica_id,
                event.process_id,
                new_process_status,
            );

            let mut builtin_table_updates = vec![builtin_table_retraction, builtin_table_addition];

            if self.controller.read_only() {
                self.buffered_builtin_table_updates
                    .as_mut()
                    .expect("in read-only mode")
                    .append(&mut builtin_table_updates);
            } else {
                self.builtin_table_update()
                    .execute(builtin_table_updates)
                    .await
                    .instrument(info_span!("coord::message_cluster_event::table_updates"))
                    .await;
            }

            let cluster = self.catalog().get_cluster(event.cluster_id);
            let replica = cluster.replica(event.replica_id).expect("Replica exists");
            let new_replica_status = self
                .cluster_replica_statuses
                .get_cluster_replica_status(event.cluster_id, event.replica_id);

            if old_replica_status != new_replica_status {
                self.broadcast_notice(AdapterNotice::ClusterReplicaStatusChanged {
                    cluster: cluster.name.clone(),
                    replica: replica.name.clone(),
                    status: new_replica_status,
                    time: event.time,
                });
            }
        }
    }

    #[mz_ore::instrument(level = "debug")]
    /// Linearizes sending the results of a read transaction by,
    ///   1. Holding back any results that were executed at some point in the future, until the
    ///   containing timeline has advanced to that point in the future.
    ///   2. Confirming that we are still the current leader before sending results to the client.
    async fn message_linearize_reads(&mut self) {
        let mut shortest_wait = Duration::from_millis(0);
        let mut ready_txns = Vec::new();

        // Cache for `TimestampOracle::read_ts` calls. These are somewhat
        // expensive so we cache the value. This is correct since all we're
        // risking is being too conservative. We will not accidentally "release"
        // a result too early.
        let mut cached_oracle_ts = BTreeMap::new();

        for (conn_id, mut read_txn) in std::mem::take(&mut self.pending_linearize_read_txns) {
            if let TimestampContext::TimelineTimestamp {
                timeline,
                chosen_ts,
                oracle_ts,
            } = read_txn.timestamp_context()
            {
                let oracle_ts = match oracle_ts {
                    Some(oracle_ts) => oracle_ts,
                    None => {
                        // There was no oracle timestamp, so no need to delay.
                        ready_txns.push(read_txn);
                        continue;
                    }
                };

                if chosen_ts <= oracle_ts {
                    // Chosen ts was already <= the oracle ts, so we're good
                    // to go!
                    ready_txns.push(read_txn);
                    continue;
                }

                // See what the oracle timestamp is now and delay when needed.
                let current_oracle_ts = cached_oracle_ts.entry(timeline.clone());
                let current_oracle_ts = match current_oracle_ts {
                    btree_map::Entry::Vacant(entry) => {
                        let timestamp_oracle = self.get_timestamp_oracle(timeline);
                        let read_ts = timestamp_oracle.read_ts().await;
                        entry.insert(read_ts.clone());
                        read_ts
                    }
                    btree_map::Entry::Occupied(entry) => entry.get().clone(),
                };

                if *chosen_ts <= current_oracle_ts {
                    ready_txns.push(read_txn);
                } else {
                    let wait =
                        Duration::from_millis(chosen_ts.saturating_sub(current_oracle_ts).into());
                    if wait < shortest_wait {
                        shortest_wait = wait;
                    }
                    read_txn.num_requeues += 1;
                    self.pending_linearize_read_txns.insert(conn_id, read_txn);
                }
            } else {
                ready_txns.push(read_txn);
            }
        }

        if !ready_txns.is_empty() {
            // Sniff out one ctx, this is where tracing breaks down because we
            // process all outstanding txns as a batch here.
            let otel_ctx = ready_txns.first().expect("known to exist").otel_ctx.clone();
            let mut span = tracing::debug_span!("message_linearize_reads");
            otel_ctx.attach_as_parent_to(&mut span);

            let now = Instant::now();
            for ready_txn in ready_txns {
                let mut span = tracing::debug_span!("retire_read_results");
                ready_txn.otel_ctx.attach_as_parent_to(&mut span);
                let _entered = span.enter();
                self.metrics
                    .linearize_message_seconds
                    .with_label_values(&[
                        ready_txn.txn.label(),
                        if ready_txn.num_requeues == 0 {
                            "true"
                        } else {
                            "false"
                        },
                    ])
                    .observe((now - ready_txn.created).as_secs_f64());
                if let Some((ctx, result)) = ready_txn.txn.finish() {
                    ctx.retire(result);
                }
            }
        }

        if !self.pending_linearize_read_txns.is_empty() {
            // Cap wait time to 1s.
            let remaining_ms = std::cmp::min(shortest_wait, Duration::from_millis(1_000));
            let internal_cmd_tx = self.internal_cmd_tx.clone();
            task::spawn(|| "deferred_read_txns", async move {
                tokio::time::sleep(remaining_ms).await;
                // It is not an error for this task to be running after `internal_cmd_rx` is dropped.
                let result = internal_cmd_tx.send(Message::LinearizeReads);
                if let Err(e) = result {
                    warn!("internal_cmd_rx dropped before we could send: {:?}", e);
                }
            });
        }
    }
}
