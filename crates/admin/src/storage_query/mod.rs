// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

mod error;
mod query;

use std::sync::Arc;

use okapi_operation::axum_integration::post;
use okapi_operation::*;

use crate::state::QueryServiceState;

pub fn create_router(state: Arc<QueryServiceState>) -> axum::Router<()> {
    // Setup the router
    axum_integration::Router::new()
        .route("/api/query", post(openapi_handler!(query::query)))
        .route_openapi_specification(
            "/api/openapi",
            OpenApiBuilder::new("Storage Query API", env!("CARGO_PKG_VERSION")),
        )
        .expect("Error when building the OpenAPI specification")
        .with_state(state)
}
