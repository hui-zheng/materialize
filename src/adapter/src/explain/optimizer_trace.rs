// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Tracing utilities for explainable plans.

use std::fmt::{Debug, Display};

use mz_compute_types::dataflows::DataflowDescription;
use mz_compute_types::plan::Plan;
use mz_expr::explain::ExplainContext;
use mz_expr::{MirRelationExpr, MirScalarExpr, OptimizedMirRelationExpr, RowSetFinishing};
use mz_ore::collections::CollectionExt;
use mz_repr::explain::tracing::{DelegateSubscriber, PlanTrace, TraceEntry};
use mz_repr::explain::{
    Explain, ExplainConfig, ExplainError, ExplainFormat, ExprHumanizer, UsedIndexes,
};
use mz_repr::{Datum, Row};
use mz_sql::plan::{self, HirRelationExpr, HirScalarExpr};
use mz_sql_parser::ast::{ExplainStage, NamedPlan};
use mz_transform::dataflow::DataflowMetainfo;
use mz_transform::notice::RawOptimizerNotice;
use smallvec::SmallVec;
use tracing::dispatcher;
use tracing_subscriber::prelude::*;

use crate::coord::peek::FastPathPlan;
use crate::explain::insights;
use crate::explain::Explainable;
use crate::AdapterError;

/// Provides functionality for tracing plans generated by the execution of an
/// optimization pipeline.
///
/// Internally, this will create a layered [`tracing::subscriber::Subscriber`]
/// consisting of one layer for each supported plan type `T`.
///
/// Use `tracing::dispatcher::set_default` to trace in synchronous context.
/// Use `tracing::instrument::WithSubscriber::with_subscriber(&optimizer_trace)` to trace the result of a `Future`.
///
/// The [`OptimizerTrace::collect_all`] method on the created instance can be
/// then used to collect the trace, and [`OptimizerTrace::collect_all`] to obtain
/// the collected trace as a vector of [`TraceEntry`] instances.
pub struct OptimizerTrace(dispatcher::Dispatch);

impl std::fmt::Debug for OptimizerTrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OptimizerTrace").finish() // Skip the dispatch field
    }
}

impl OptimizerTrace {
    /// Create a new [`OptimizerTrace`].
    ///
    /// The instance will only accumulate [`TraceEntry`] instances along
    /// the prefix of the given `path` if `path` is present, or it will
    /// accumulate all [`TraceEntry`] instances otherwise.
    pub fn new(broken: bool, filter: Option<SmallVec<[NamedPlan; 4]>>) -> OptimizerTrace {
        let filter = || filter.clone();
        if broken {
            let subscriber = DelegateSubscriber::default()
                // Collect `explain_plan` types that are not used in the regular explain
                // path, but are useful when instrumenting code for debugging purposes.
                .with(PlanTrace::<String>::new(filter()))
                .with(PlanTrace::<HirScalarExpr>::new(filter()))
                .with(PlanTrace::<MirScalarExpr>::new(filter()))
                // Collect `explain_plan` types that are used in the regular explain path.
                .with(PlanTrace::<HirRelationExpr>::new(filter()))
                .with(PlanTrace::<MirRelationExpr>::new(filter()))
                .with(PlanTrace::<DataflowDescription<OptimizedMirRelationExpr>>::new(filter()))
                .with(PlanTrace::<DataflowDescription<Plan>>::new(filter()))
                // Don't filter for FastPathPlan entries (there can be at most one).
                .with(PlanTrace::<FastPathPlan>::new(None))
                .with(PlanTrace::<UsedIndexes>::new(None));

            OptimizerTrace(dispatcher::Dispatch::new(subscriber))
        } else {
            let subscriber = tracing_subscriber::registry()
                // Collect `explain_plan` types that are not used in the regular explain
                // path, but are useful when instrumenting code for debugging purposes.
                .with(PlanTrace::<String>::new(filter()))
                .with(PlanTrace::<HirScalarExpr>::new(filter()))
                .with(PlanTrace::<MirScalarExpr>::new(filter()))
                // Collect `explain_plan` types that are used in the regular explain path.
                .with(PlanTrace::<HirRelationExpr>::new(filter()))
                .with(PlanTrace::<MirRelationExpr>::new(filter()))
                .with(PlanTrace::<DataflowDescription<OptimizedMirRelationExpr>>::new(filter()))
                .with(PlanTrace::<DataflowDescription<Plan>>::new(filter()))
                // Don't filter for FastPathPlan entries (there can be at most one).
                .with(PlanTrace::<FastPathPlan>::new(None))
                .with(PlanTrace::<UsedIndexes>::new(None));

            OptimizerTrace(dispatcher::Dispatch::new(subscriber))
        }
    }

    /// Convert the optimizer trace into a vector or rows that can be returned
    /// to the client.
    pub fn into_rows(
        self,
        format: ExplainFormat,
        config: &ExplainConfig,
        humanizer: &dyn ExprHumanizer,
        row_set_finishing: Option<RowSetFinishing>,
        target_cluster: Option<&str>,
        dataflow_metainfo: DataflowMetainfo,
        stage: ExplainStage,
        stmt_kind: plan::ExplaineeStatementKind,
    ) -> Result<Vec<Row>, AdapterError> {
        let collect_all = |format| {
            self.collect_all(
                format,
                config,
                humanizer,
                row_set_finishing.clone(),
                target_cluster,
                dataflow_metainfo.clone(),
            )
        };

        let rows = match stage {
            ExplainStage::Trace => {
                // For the `Trace` (pseudo-)stage, return the entire trace as
                // triples of (time, path, plan) values.
                let rows = collect_all(format)?
                    .0
                    .into_iter()
                    .map(|entry| {
                        // The trace would have to take over 584 years to overflow a u64.
                        let span_duration = u64::try_from(entry.span_duration.as_nanos());
                        Row::pack_slice(&[
                            Datum::from(span_duration.unwrap_or(u64::MAX)),
                            Datum::from(entry.path.as_str()),
                            Datum::from(entry.plan.as_str()),
                        ])
                    })
                    .collect();
                rows
            }
            ExplainStage::PlanInsights => {
                if format != ExplainFormat::Json {
                    coord_bail!("EXPLAIN PLAN INSIGHTS only supports JSON format");
                }

                let mut text_traces = collect_all(ExplainFormat::Text)?;
                let mut json_traces = collect_all(ExplainFormat::Json)?;
                let global_plan = self.collect_global_plan();
                let fast_path_plan = self.collect_fast_path_plan();

                let mut get_plan = |name: NamedPlan| {
                    let text_plan = match text_traces.remove(name.path()) {
                        None => "<unknown>".into(),
                        Some(entry) => entry.plan,
                    };
                    let json_plan = match json_traces.remove(name.path()) {
                        None => serde_json::Value::Null,
                        Some(entry) => serde_json::from_str(&entry.plan).map_err(|e| {
                            AdapterError::Unstructured(anyhow::anyhow!("internal error: {e}"))
                        })?,
                    };
                    Ok::<_, AdapterError>(serde_json::json!({
                        "text": text_plan,
                        "json": json_plan,
                    }))
                };

                let output = serde_json::json!({
                    "plans": {
                        "raw": get_plan(NamedPlan::Raw)?,
                        "optimized": {
                            "global": get_plan(NamedPlan::Global)?,
                            "fast_path": get_plan(NamedPlan::FastPath)?,
                        }
                    },
                    "insights": insights::plan_insights(humanizer, global_plan, fast_path_plan),
                });
                let output = serde_json::to_string_pretty(&output).expect("JSON string");
                vec![Row::pack_slice(&[Datum::from(output.as_str())])]
            }
            _ => {
                // For everything else, return the plan for the stage identified
                // by the corresponding path.

                let path = stage
                    .paths()
                    .map(|path| path.into_element().path())
                    .ok_or_else(|| {
                        AdapterError::Internal("explain stage unexpectedly missing path".into())
                    })?;
                let mut traces = collect_all(format)?;

                // For certain stages we want to return the resulting fast path
                // plan instead of the selected stage if it is present.
                let plan = if stage.show_fast_path() && !config.no_fast_path {
                    traces
                        .remove(NamedPlan::FastPath.path())
                        .or_else(|| traces.remove(path))
                } else {
                    traces.remove(path)
                };

                let row = plan
                    .map(|entry| Row::pack_slice(&[Datum::from(entry.plan.as_str())]))
                    .ok_or_else(|| {
                        if !stmt_kind.supports(&stage) {
                            // Print a nicer error for unsupported stages.
                            AdapterError::Unstructured(anyhow::anyhow!(format!(
                                "cannot EXPLAIN {stage} FOR {stmt_kind}"
                            )))
                        } else {
                            // We don't expect this stage to be missing.
                            AdapterError::Internal(format!(
                                "stage `{path}` not present in the collected optimizer trace",
                            ))
                        }
                    })?;
                vec![row]
            }
        };

        Ok(rows)
    }

    pub fn into_plan_insights(
        self,
        humanizer: &dyn ExprHumanizer,
        row_set_finishing: Option<RowSetFinishing>,
        target_cluster: Option<&str>,
        dataflow_metainfo: DataflowMetainfo,
    ) -> Result<String, AdapterError> {
        let rows = self.into_rows(
            ExplainFormat::Json,
            &ExplainConfig::default(),
            humanizer,
            row_set_finishing,
            target_cluster,
            dataflow_metainfo,
            ExplainStage::PlanInsights,
            plan::ExplaineeStatementKind::Select,
        )?;

        // When using `ExplainStage::PlanInsights`, we're guaranteed that the
        // output is a single row containing a single column containing the plan
        // insights as a string.
        Ok(rows.into_element().into_element().unwrap_str().into())
    }

    /// Collect all traced plans for all plan types `T` that are available in
    /// the wrapped [`dispatcher::Dispatch`].
    pub fn collect_all(
        &self,
        format: ExplainFormat,
        config: &ExplainConfig,
        humanizer: &dyn ExprHumanizer,
        row_set_finishing: Option<RowSetFinishing>,
        target_cluster: Option<&str>,
        dataflow_metainfo: DataflowMetainfo,
    ) -> Result<TraceEntries<String>, ExplainError> {
        let mut results = vec![];

        // First, create an ExplainContext without `used_indexes`. We'll use this to, e.g., collect
        // HIR plans.
        let mut context = ExplainContext {
            config,
            humanizer,
            used_indexes: Default::default(),
            finishing: row_set_finishing.clone(),
            duration: Default::default(),
            target_cluster,
            optimizer_notices: RawOptimizerNotice::explain(
                &dataflow_metainfo.optimizer_notices,
                humanizer,
                config.redacted,
            )?,
        };

        // Collect trace entries of types produced by local optimizer stages.
        results.extend(itertools::chain!(
            self.collect_explainable_entries::<HirRelationExpr>(&format, &mut context)?,
            self.collect_explainable_entries::<MirRelationExpr>(&format, &mut context)?,
        ));

        // Collect trace entries of types produced by global optimizer stages.
        let mut context = ExplainContext {
            config,
            humanizer,
            used_indexes: Default::default(),
            finishing: row_set_finishing,
            duration: Default::default(),
            target_cluster,
            optimizer_notices: RawOptimizerNotice::explain(
                &dataflow_metainfo.optimizer_notices,
                humanizer,
                config.redacted,
            )?,
        };
        results.extend(itertools::chain!(
            self.collect_explainable_entries::<DataflowDescription<OptimizedMirRelationExpr>>(
                &format,
                &mut context,
            )?,
            self.collect_explainable_entries::<DataflowDescription<Plan>>(&format, &mut context)?,
            self.collect_explainable_entries::<FastPathPlan>(&format, &mut context)?,
        ));

        // Collect trace entries of type String, HirScalarExpr, MirScalarExpr
        // which are useful for ad-hoc debugging.
        results.extend(itertools::chain!(
            self.collect_scalar_entries::<HirScalarExpr>(),
            self.collect_scalar_entries::<MirScalarExpr>(),
            self.collect_string_entries(),
        ));

        // sort plans by instant (TODO: this can be implemented in a more
        // efficient way, as we can assume that each of the runs that are used
        // to `*.extend` the `results` vector is already sorted).
        results.sort_by_key(|x| x.instant);

        Ok(TraceEntries(results))
    }

    /// Collects the global optimized plan from the trace, if it exists.
    pub fn collect_global_plan(&self) -> Option<DataflowDescription<OptimizedMirRelationExpr>> {
        self.0
            .downcast_ref::<PlanTrace<DataflowDescription<OptimizedMirRelationExpr>>>()
            .and_then(|trace| trace.find(NamedPlan::Global.path()))
            .map(|entry| entry.plan)
    }

    /// Collects the fast path plan from the trace, if it exists.
    pub fn collect_fast_path_plan(&self) -> Option<FastPathPlan> {
        self.0
            .downcast_ref::<PlanTrace<FastPathPlan>>()
            .and_then(|trace| trace.find(NamedPlan::FastPath.path()))
            .map(|entry| entry.plan)
    }

    /// Collect all trace entries of a plan type `T` that implements
    /// [`Explainable`].
    fn collect_explainable_entries<T>(
        &self,
        format: &ExplainFormat,
        context: &mut ExplainContext,
    ) -> Result<Vec<TraceEntry<String>>, ExplainError>
    where
        T: Clone + Debug + 'static,
        for<'a> Explainable<'a, T>: Explain<'a, Context = ExplainContext<'a>>,
    {
        if let Some(trace) = self.0.downcast_ref::<PlanTrace<T>>() {
            // Get a handle of the associated `PlanTrace<UsedIndexes>`.
            let used_indexes_trace = self.0.downcast_ref::<PlanTrace<UsedIndexes>>();

            trace
                .collect_as_vec()
                .into_iter()
                .map(|mut entry| {
                    // Update the context with the current time.
                    context.duration = entry.full_duration;

                    // Try to find the UsedIndexes instance for this entry.
                    let used_indexes = used_indexes_trace.map(|t| t.used_indexes_for(&entry.path));

                    // Render the EXPLAIN output string for this entry.
                    let plan = if let Some(mut used_indexes) = used_indexes {
                        // Temporary swap the found UsedIndexes with the default
                        // one in the ExplainContext while explaining the plan
                        // for this entry.
                        std::mem::swap(&mut context.used_indexes, &mut used_indexes);
                        let plan = Explainable::new(&mut entry.plan).explain(format, context)?;
                        std::mem::swap(&mut context.used_indexes, &mut used_indexes);
                        plan
                    } else {
                        // No UsedIndexes instance for this entry found - use
                        // the default UsedIndexes in the ExplainContext.
                        Explainable::new(&mut entry.plan).explain(format, context)?
                    };

                    Ok(TraceEntry {
                        instant: entry.instant,
                        span_duration: entry.span_duration,
                        full_duration: entry.full_duration,
                        path: entry.path,
                        plan,
                    })
                })
                .collect()
        } else {
            unreachable!("collect_explainable_entries called with wrong plan type T");
        }
    }

    /// Collect all trace entries of a plan type `T`.
    fn collect_scalar_entries<T>(&self) -> Vec<TraceEntry<String>>
    where
        T: Clone + Debug + 'static,
        T: Display,
    {
        if let Some(trace) = self.0.downcast_ref::<PlanTrace<T>>() {
            trace
                .collect_as_vec()
                .into_iter()
                .map(|entry| TraceEntry {
                    instant: entry.instant,
                    span_duration: entry.span_duration,
                    full_duration: entry.full_duration,
                    path: entry.path,
                    plan: entry.plan.to_string(),
                })
                .collect()
        } else {
            vec![]
        }
    }

    /// Collect all trace entries with plans of type [`String`].
    fn collect_string_entries(&self) -> Vec<TraceEntry<String>> {
        if let Some(trace) = self.0.downcast_ref::<PlanTrace<String>>() {
            trace.collect_as_vec()
        } else {
            vec![]
        }
    }
}

impl From<&OptimizerTrace> for tracing::Dispatch {
    fn from(value: &OptimizerTrace) -> Self {
        // be not afraid: value.0 is a Dispatcher, which is Arc<dyn Subscriber + ...>
        // https://docs.rs/tracing-core/0.1.30/src/tracing_core/dispatcher.rs.html#451-453
        value.0.clone()
    }
}

/// A collection of optimizer trace entries with convenient accessor methods.
pub struct TraceEntries<T>(pub Vec<TraceEntry<T>>);

impl<T> TraceEntries<T> {
    // Removes the first (and by assumption the only) trace that matches the
    // given path from the collected trace.
    pub fn remove(&mut self, path: &'static str) -> Option<TraceEntry<T>> {
        let index = self.0.iter().position(|entry| entry.path == path);
        index.map(|index| self.0.remove(index))
    }
}
