// Copyright 2018 sqlparser-rs contributors. All rights reserved.
// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// This file is derived from the sqlparser-rs project, available at
// https://github.com/andygrove/sqlparser-rs. It was incorporated
// directly into Materialize on December 21, 2019.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License in the LICENSE file at the
// root of this repository, or online at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::collapsible_if)]
#![warn(clippy::collapsible_else_if)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG

use std::sync::Arc;

use tokio::sync::Mutex;

use mz_adapter::catalog::{Catalog, CatalogItem, Op, Table, SYSTEM_CONN_ID};
use mz_adapter::session::{Session, DEFAULT_DATABASE_NAME};
use mz_ore::now::NOW_ZERO;
use mz_repr::RelationDesc;
use mz_sql::ast::{Expr, Statement};
use mz_sql::catalog::CatalogDatabase;
use mz_sql::names::{self, ObjectQualifiers, QualifiedObjectName, ResolvedDatabaseSpecifier};
use mz_sql::plan::{PlanContext, QueryContext, QueryLifetime, StatementContext};
use mz_sql::DEFAULT_SCHEMA;

// This morally tests the name resolution stuff, but we need access to a
// catalog.

#[tokio::test]
async fn datadriven() {
    datadriven::walk_async("tests/testdata", |mut f| async {
        // The datadriven API takes an `FnMut` closure, and can't express to Rust that
        // it will finish polling each returned future before calling the closure
        // again, so we have to wrap the catalog in a share-able type. Datadriven
        // could be changed to maybe take some context that it passes as a &mut to each
        // test_case invocation, act on a stream of test_cases, or take and return a
        // Context. This is just a test, so the performance hit of this doesn't matter
        // (and in practice there will be no contention).
        let catalog = Arc::new(Mutex::new(
            Catalog::open_debug(NOW_ZERO.clone()).await.unwrap(),
        ));
        f.run_async(|test_case| {
            let catalog = Arc::clone(&catalog);
            async move {
                let mut catalog = catalog.lock().await;
                match test_case.directive.as_str() {
                    "add-table" => {
                        let id = catalog.allocate_user_id().await.unwrap();
                        let oid = catalog.allocate_oid().unwrap();
                        let database_id = catalog
                            .resolve_database(DEFAULT_DATABASE_NAME)
                            .unwrap()
                            .id();
                        let database_spec = ResolvedDatabaseSpecifier::Id(database_id);
                        let schema_spec = catalog
                            .resolve_schema_in_database(
                                &database_spec,
                                DEFAULT_SCHEMA,
                                SYSTEM_CONN_ID,
                            )
                            .unwrap()
                            .id
                            .clone();
                        catalog
                            .transact(
                                mz_repr::Timestamp::MIN,
                                None,
                                vec![Op::CreateItem {
                                    id,
                                    oid,
                                    name: QualifiedObjectName {
                                        qualifiers: ObjectQualifiers {
                                            database_spec,
                                            schema_spec,
                                        },
                                        item: test_case.input.trim_end().to_string(),
                                    },
                                    item: CatalogItem::Table(Table {
                                        create_sql: "TODO".to_string(),
                                        desc: RelationDesc::empty(),
                                        defaults: vec![Expr::null(); 0],
                                        conn_id: None,
                                        depends_on: vec![],
                                        custom_logical_compaction_window: None,
                                        is_retained_metrics_relation: false,
                                    }),
                                }],
                                |_| Ok(()),
                            )
                            .await
                            .unwrap();
                        format!("{}\n", id)
                    }
                    "resolve" => {
                        let sess = Session::dummy();
                        let catalog = catalog.for_session(&sess);

                        let parsed = mz_sql::parse::parse(&test_case.input).unwrap();
                        let pcx = &PlanContext::zero();
                        let scx = StatementContext::new(Some(pcx), &catalog);
                        let qcx =
                            QueryContext::root(&scx, QueryLifetime::OneShot(scx.pcx().unwrap()));
                        let q = parsed[0].clone();
                        let q = match q {
                            Statement::Select(s) => s.query,
                            _ => unreachable!(),
                        };
                        let resolved = names::resolve(qcx.scx.catalog, q);
                        match resolved {
                            Ok((q, _depends_on)) => format!("{}\n", q),
                            Err(e) => format!("error: {}\n", e),
                        }
                    }
                    dir => panic!("unhandled directive {}", dir),
                }
            }
        })
        .await;
        f
    })
    .await;
}
