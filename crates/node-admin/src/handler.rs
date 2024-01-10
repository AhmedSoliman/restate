// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use axum::extract::State;
use tonic::{Request, Response, Status};

use restate_node_admin_proto::proto::node_admin_server::NodeAdmin;
use restate_node_admin_proto::proto::{IdentResponse, NodeStatus};

use crate::state::NodeAdminHandlerState;

// -- Direct HTTP Handlers --
pub async fn render_metrics(
    State(state): State<NodeAdminHandlerState>,
) -> (http::StatusCode, String) {
    // Response content type is plain/text and that's expected.
    if let Some(prometheus_handle) = state.prometheus_handle {
        (http::StatusCode::OK, prometheus_handle.render())
    } else {
        // We want to fail scraping to prevent silent failures.
        (
            // We respond with 422 since this is technically not a server error.
            // We indicate that that the request is valid but cannot process this
            // request due to semantic errors (i.e. not enabled in this case).
            http::StatusCode::UNPROCESSABLE_ENTITY,
            "Prometheus metric collection is not enabled.".to_string(),
        )
    }
}

// -- GRPC Service Handlers --
pub struct NodeAdminHandler {
    #[allow(dead_code)]
    state: NodeAdminHandlerState,
}

impl NodeAdminHandler {
    pub fn new(state: NodeAdminHandlerState) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl NodeAdmin for NodeAdminHandler {
    async fn get_ident(&self, _request: Request<()>) -> Result<Response<IdentResponse>, Status> {
        // STUB IMPLEMENTATION
        return Ok(Response::new(IdentResponse {
            status: NodeStatus::Alive.into(),
        }));
    }
}
