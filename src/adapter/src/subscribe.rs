// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Implementations around supporting the SUBSCRIBE protocol with the dataflow layer

use std::collections::BTreeSet;

use mz_compute_client::types::sinks::SinkAsOf;
use mz_ore::now::EpochMillis;
use timely::progress::Antichain;
use tokio::sync::mpsc;

use mz_compute_client::protocol::response::{SubscribeBatch, SubscribeResponse};
use mz_controller::clusters::ClusterId;
use mz_repr::adt::numeric;
use mz_repr::{Datum, GlobalId, Row, Timestamp};
use mz_sql::session::user::User;

use crate::client::ConnectionId;
use crate::coord::peek::PeekResponseUnary;

/// A description of an active subscribe from coord's perspective
#[derive(Debug)]
pub struct ActiveSubscribe {
    /// The user of the session that created the subscribe.
    pub user: User,
    /// The connection id of the session that created the subscribe.
    pub conn_id: ConnectionId,
    /// Channel to send responses to the client.
    ///
    /// The responses have the form `PeekResponseUnary` but should perhaps become `TailResponse`.
    pub channel: mpsc::UnboundedSender<PeekResponseUnary>,
    /// Whether progress information should be emitted.
    pub emit_progress: bool,
    /// As of of subscribe
    pub as_of: SinkAsOf,
    /// Number of columns in the output.
    pub arity: usize,
    /// The cluster that the subscribe is running on.
    pub cluster_id: ClusterId,
    /// All `GlobalId`s that the subscribe depend on.
    pub depends_on: BTreeSet<GlobalId>,
    /// The time when the subscribe was started.
    pub start_time: EpochMillis,
    /// Whether we are already in the process of dropping the resources related to this subscribe.
    pub dropping: bool,
}

impl ActiveSubscribe {
    pub(crate) fn initialize(&self) {
        // Always emit progress message indicating snapshot timestamp.
        if self.emit_progress {
            self.send_progress_message(&self.as_of.frontier);
        }
    }

    fn send_progress_message(&self, upper: &Antichain<Timestamp>) {
        if !upper.is_empty() {
            assert_eq!(
                upper.len(),
                1,
                "SUBSCRIBE only supports single-dimensional timestamps"
            );
            let mut row_buf = Row::default();
            let mut packer = row_buf.packer();
            packer.push(Datum::from(numeric::Numeric::from(upper[0])));
            packer.push(Datum::True);
            // Fill in the diff column and all table columns with NULL.
            for _ in 0..(self.arity + 1) {
                packer.push(Datum::Null);
            }

            let result = self.channel.send(PeekResponseUnary::Rows(vec![row_buf]));
            if result.is_err() {
                // TODO(benesch): we should actually drop the sink if the
                // receiver has gone away. E.g. form a DROP SINK command?
            }
        }
    }

    /// Process a subscribe response
    ///
    /// Returns `true` if the sink should be removed.
    pub(crate) fn process_response(&mut self, response: SubscribeResponse) -> bool {
        let mut row_buf = Row::default();
        match response {
            SubscribeResponse::Batch(SubscribeBatch {
                lower: _,
                upper,
                updates,
            }) => {
                match updates {
                    Ok(mut rows) => {
                        // Sort results by time. We use stable sort here because it will produce deterministic
                        // results since the cursor will always produce rows in the same order.
                        // TODO: Is sorting necessary?
                        rows.sort_by_key(|(time, _, _)| *time);

                        let rows = rows
                            .into_iter()
                            .map(|(time, row, diff)| {
                                if self.as_of.strict {
                                    assert!(self.as_of.frontier.less_than(&time));
                                } else {
                                    assert!(self.as_of.frontier.less_equal(&time));
                                }
                                let mut packer = row_buf.packer();
                                // TODO: Change to MzTimestamp.
                                packer.push(Datum::from(numeric::Numeric::from(time)));
                                if self.emit_progress {
                                    // When sinking with PROGRESS, the output
                                    // includes an additional column that
                                    // indicates whether a timestamp is
                                    // complete. For regular "data" updates this
                                    // is always `false`.
                                    packer.push(Datum::False);
                                }

                                packer.push(Datum::Int64(diff));

                                packer.extend_by_row(&row);

                                row_buf.clone()
                            })
                            .collect();
                        // TODO(benesch): the lack of backpressure here can result in
                        // unbounded memory usage.
                        let result = self.channel.send(PeekResponseUnary::Rows(rows));
                        if result.is_err() {
                            // TODO(benesch): we should actually drop the sink if the
                            // receiver has gone away. E.g. form a DROP SINK command?
                        }
                    }
                    Err(text) => {
                        let result = self.channel.send(PeekResponseUnary::Error(text));
                        if result.is_err() {
                            // TODO(benesch): we should actually drop the sink if the
                            // receiver has gone away. E.g. form a DROP SINK command?
                        }
                    }
                }
                // Emit progress message if requested. Don't emit progress for the first batch if the upper
                // is exactly `as_of` (we're guaranteed it is not less than `as_of`, but it might be exactly
                // `as_of`) as we've already emitted that progress message in `initialize`.
                if self.emit_progress && upper != self.as_of.frontier {
                    self.send_progress_message(&upper);
                }
                upper.is_empty()
            }
            SubscribeResponse::DroppedAt(_frontier) => {
                // TODO: Could perhaps do this earlier, in response to DROP SINK.
                true
            }
        }
    }
}
