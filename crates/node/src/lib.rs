// Copyright (c) 2024 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

mod options;
mod roles;
mod server;

use codederror::CodedError;
use restate_types::time::MillisSinceEpoch;
use restate_types::{GenerationalNodeId, MyNodeIdWriter, NodeId, PlainNodeId, Version};
use std::str::FromStr;
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;
use tracing::{info, warn};

use crate::roles::{AdminRole, WorkerRole};
use crate::server::{ClusterControllerDependencies, NodeServer, WorkerDependencies};
pub use options::{Options, OptionsBuilder as NodeOptionsBuilder};
pub use restate_admin::OptionsBuilder as AdminOptionsBuilder;
pub use restate_meta::OptionsBuilder as MetaOptionsBuilder;
use restate_node_services::cluster_controller::cluster_controller_svc_client::ClusterControllerSvcClient;
use restate_node_services::cluster_controller::AttachmentRequest;
use restate_task_center::{task_center, TaskKind};
use restate_types::nodes_config::{AdvertisedAddress, NodeConfig, NodesConfiguration, Role};
use restate_types::retries::RetryPolicy;
pub use restate_worker::{OptionsBuilder as WorkerOptionsBuilder, RocksdbOptionsBuilder};

#[derive(Debug, thiserror::Error, CodedError)]
pub enum Error {
    #[error("invalid cluster controller address: {0}")]
    #[code(unknown)]
    InvalidClusterControllerAddress(http::Error),
    #[error("failed to attach to cluster at '{0}': {1}")]
    #[code(unknown)]
    Attachment(AdvertisedAddress, tonic::Status),
}

#[derive(Debug, thiserror::Error, CodedError)]
pub enum BuildError {
    #[error("building worker failed: {0}")]
    Worker(
        #[from]
        #[code]
        roles::WorkerRoleBuildError,
    ),
    #[error("building cluster controller failed: {0}")]
    ClusterController(
        #[from]
        #[code]
        roles::AdminRoleBuildError,
    ),
    #[error("node neither runs cluster controller nor its address has been configured")]
    #[code(unknown)]
    UnknownClusterController,

    #[error("cluster bootstrap failed: {0}")]
    #[code(unknown)]
    Bootstrap(String),
}

pub struct Node {
    options: Options,
    admin_address: AdvertisedAddress,

    admin_role: Option<AdminRole>,
    worker_role: Option<WorkerRole>,
    server: NodeServer,
}

impl Node {
    pub fn new(options: Options) -> Result<Self, BuildError> {
        let opts = options.clone();
        // ensure we have cluster admin role if bootstrapping.
        if options.bootstrap_cluster {
            info!("Bootstrapping cluster");
            if !options.roles.contains(Role::Admin) {
                return Err(BuildError::Bootstrap(format!(
                    "Node must include the 'Admin' role when starting in bootstrap mode. Currently it has roles {}", options.roles
                )));
            }
        }

        let admin_role = if options.roles.contains(Role::Admin) {
            Some(AdminRole::try_from(options.clone())?)
        } else {
            None
        };

        let worker_role = if options.roles.contains(Role::Worker) {
            Some(WorkerRole::try_from(options.clone())?)
        } else {
            None
        };

        let server = options.server.build(
            worker_role.as_ref().map(|worker| {
                WorkerDependencies::new(
                    worker.rocksdb_storage().clone(),
                    worker.bifrost_handle(),
                    worker.worker_command_tx(),
                    worker.storage_query_context().clone(),
                    worker.schemas(),
                    worker.subscription_controller(),
                )
            }),
            admin_role.as_ref().map(|cluster_controller| {
                ClusterControllerDependencies::new(
                    cluster_controller.cluster_controller_handle(),
                    cluster_controller.schema_reader(),
                )
            }),
        );

        let admin_address = if let Some(admin_address) = options.admin_address {
            if admin_role.is_some() {
                warn!("This node is running the admin roles but has also a remote admin address configured. \
                This indicates a potential misconfiguration. Trying to connect to the remote admin.");
            }

            admin_address
        } else if admin_role.is_some() {
            AdvertisedAddress::from_str(&format!("http://127.0.0.1:{}/", server.port()))
                .expect("valid local address")
        } else {
            return Err(BuildError::UnknownClusterController);
        };

        Ok(Node {
            options: opts,
            admin_address,
            admin_role,
            worker_role,
            server,
        })
    }

    pub async fn start(self) -> Result<(), anyhow::Error> {
        let tc = task_center();
        // If starting in bootstrap mode, we initialize the nodes configuration
        // with a static config.
        if self.options.bootstrap_cluster {
            let temp_id: GenerationalNodeId = if let Some(my_id) = self.options.node_id {
                my_id.with_generation(1)
            } else {
                // default to node-id 1 generation 1
                GenerationalNodeId::new(1, 1)
            };
            // Temporary: nodes configuration from current node.
            let mut nodes_config =
                NodesConfiguration::new(Version::MIN, self.options.cluster_name.clone());
            let address = self.options.server.advertise_address.clone();

            let my_node = NodeConfig::new(
                self.options.node_name.clone(),
                temp_id,
                address,
                self.options.roles,
            );
            nodes_config.upsert_node(my_node);
            info!(
                "Created a bootstrap nodes-configuration version {} for cluster {}",
                nodes_config.version(),
                self.options.cluster_name.clone(),
            );
            info!("Initial nodes configuration is loaded");
        } else {
            // Not supported at the moment
            unimplemented!()
        }

        if let Some(admin_role) = self.admin_role {
            tc.spawn(
                TaskKind::SystemBoot,
                "admin-init",
                None,
                admin_role.start(self.options.bootstrap_cluster),
            )?;
        }

        tc.spawn(
            TaskKind::RpcServer,
            "node-rpc-server",
            None,
            self.server.run(),
        )?;

        Self::attach_node(self.options, self.admin_address).await?;

        if let Some(worker_role) = self.worker_role {
            tc.spawn(TaskKind::SystemBoot, "worker-init", None, async {
                // MyNodeId should be set here.
                // Startup the worker role.
                worker_role
                    .start(
                        NodeId::my_node_id()
                            .expect("my NodeId should be set after attaching to cluster"),
                    )
                    .await?;
                Ok(())
            })?;
        }

        Ok(())
    }

    async fn attach_node(options: Options, admin_address: AdvertisedAddress) -> Result<(), Error> {
        info!(
            "Attaching '{}' (insist on ID?={:?}) to admin at '{admin_address}'",
            options.node_name, options.node_id,
        );

        let channel = Self::create_channel_from_network_address(&admin_address)
            .map_err(Error::InvalidClusterControllerAddress)?;

        let cc_client = ClusterControllerSvcClient::new(channel);

        let _response = RetryPolicy::exponential(Duration::from_millis(50), 2.0, 10, None)
            .retry_operation(|| async {
                cc_client
                    .clone()
                    .attach_node(AttachmentRequest {
                        node_id: options.node_id.map(Into::into),
                        node_name: options.node_name.clone(),
                    })
                    .await
            })
            .await
            .map_err(|err| Error::Attachment(admin_address, err))?;

        // todo: Generational NodeId should come from attachment result
        let now = MillisSinceEpoch::now();
        let my_node_id: NodeId = options
            .node_id
            .unwrap_or(PlainNodeId::from(1))
            .with_generation(now.as_u64() as u32)
            .into();
        // We are attached, we can set our own NodeId.
        MyNodeIdWriter::set_as_my_node_id(my_node_id);
        info!(
            "Node attached to cluster controller. My Node ID is {}",
            my_node_id
        );
        Ok(())
    }

    fn create_channel_from_network_address(
        cluster_controller_address: &AdvertisedAddress,
    ) -> Result<Channel, http::Error> {
        let channel = match cluster_controller_address {
            AdvertisedAddress::Uds(uds_path) => {
                let uds_path = uds_path.clone();
                // dummy endpoint required to specify an uds connector, it is not used anywhere
                Endpoint::try_from("/")
                    .expect("/ should be a valid Uri")
                    .connect_with_connector_lazy(service_fn(move |_: Uri| {
                        UnixStream::connect(uds_path.clone())
                    }))
            }
            AdvertisedAddress::Http(uri) => Self::create_lazy_channel_from_uri(uri.clone()),
        };
        Ok(channel)
    }

    fn create_lazy_channel_from_uri(uri: Uri) -> Channel {
        // todo: Make the channel settings configurable
        Channel::builder(uri)
            .connect_timeout(Duration::from_secs(5))
            .connect_lazy()
    }
}
