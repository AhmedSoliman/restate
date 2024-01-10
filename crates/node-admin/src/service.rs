// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::net::SocketAddr;

use axum::routing::get;
use codederror::CodedError;
use futures::FutureExt;
use tower_http::trace::TraceLayer;
use tracing::info;

use restate_node_admin_proto::proto::node_admin_server::NodeAdminServer;
use restate_node_admin_proto::proto::FILE_DESCRIPTOR_SET;

use crate::handler::NodeAdminHandler;
use crate::multiplex::MultiplexService;
use crate::state::NodeAdminHandlerStateBuilder;
use crate::Options;

#[derive(Debug, thiserror::Error, CodedError)]
pub enum Error {
    #[error("failed binding to address '{address}' specified in 'node_admin.bind_address'")]
    #[code(restate_errors::RT0004)]
    Binding {
        address: SocketAddr,
        #[source]
        source: hyper::Error,
    },
    #[error("error while running node's admin server: {0}")]
    #[code(unknown)]
    Running(hyper::Error),
    #[error("error while running node's admin server grpc reflection service: {0}")]
    #[code(unknown)]
    Grpc(#[from] tonic_reflection::server::Error),
}

pub struct NodeAdminService {
    opts: Options,
}

impl NodeAdminService {
    pub fn new(opts: Options) -> Self {
        Self { opts }
    }

    pub async fn run(self, drain: drain::Watch) -> Result<(), Error> {
        // Configure Metric Exporter
        let mut state_builder = NodeAdminHandlerStateBuilder::default();

        if !self.opts.disable_prometheus {
            state_builder.prometheus_handle(Some(
                crate::metrics::install_global_prometheus_recorder(&self.opts),
            ));
        }

        let shared_state = state_builder.build().unwrap();
        // Trace layer
        let span_factory = tower_http::trace::DefaultMakeSpan::new()
            .include_headers(true)
            .level(tracing::Level::ERROR);

        // -- HTTP service (for prometheus et al.)
        let router = axum::Router::new()
            .route("/metrics", get(crate::handler::render_metrics))
            .with_state(shared_state.clone())
            .layer(TraceLayer::new_for_http().make_span_with(span_factory.clone()))
            .fallback(handler_404);

        // -- GRPC Service Setup
        let tonic_reflection_service = tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
            .build()?;

        // Build the NodeAdmin grpc service
        let grpc = tonic::transport::Server::builder()
            .layer(TraceLayer::new_for_grpc().make_span_with(span_factory))
            .add_service(NodeAdminServer::new(NodeAdminHandler::new(shared_state)))
            .add_service(tonic_reflection_service)
            .into_service();

        // Multiplex both grpc and http based on content-type
        let service = MultiplexService::new(router, grpc);

        // Bind and serve
        let server = hyper::Server::try_bind(&self.opts.bind_address)
            .map_err(|err| Error::Binding {
                address: self.opts.bind_address,
                source: err,
            })?
            .serve(tower::make::Shared::new(service));

        info!(
            net.host.addr = %server.local_addr().ip(),
            net.host.port = %server.local_addr().port(),
            "Node Admin service listening"
        );

        // Wait server graceful shutdown
        server
            .with_graceful_shutdown(drain.signaled().map(|_| ()))
            .await
            .map_err(Error::Running)
    }
}

// handle 404
async fn handler_404() -> (http::StatusCode, &'static str) {
    (
        http::StatusCode::NOT_FOUND,
        "Are you lost? Maybe visit https://restate.dev instead!",
    )
}
