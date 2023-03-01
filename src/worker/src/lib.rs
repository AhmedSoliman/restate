use crate::network_integration::FixedPartitionTable;
use common::types::{PeerId, PeerTarget};
use consensus::{Consensus, ProposalSender};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use invoker::{EndpointMetadata, Invoker, UnboundedInvokerInputSender};
use network::UnboundedNetworkHandle;
use partition::shuffle;
use partition::RocksDBJournalReader;
use service_protocol::codec::ProtobufRawEntryCodec;
use std::collections::HashMap;
use storage_rocksdb::RocksDBStorage;
use tokio::join;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::PollSender;
use tracing::debug;
use util::IdentitySender;

mod ingress_integration;
mod network_integration;
mod partition;
mod util;

type ConsensusCommand = consensus::Command<partition::Command>;
type ConsensusMsg = PeerTarget<partition::Command>;
type PartitionProcessor = partition::PartitionProcessor<
    ReceiverStream<ConsensusCommand>,
    IdentitySender<partition::Command>,
    ProtobufRawEntryCodec,
    UnboundedInvokerInputSender,
    UnboundedNetworkHandle<shuffle::ShuffleInput, shuffle::ShuffleOutput>,
    RocksDBStorage,
>;

#[derive(Debug, clap::Parser)]
#[group(skip)]
pub struct Options {
    /// Bounded channel size
    #[arg(
        long = "worker-channel-size",
        env = "WORKER_CHANNEL_SIZE",
        default_value = "64"
    )]
    channel_size: usize,

    #[command(flatten)]
    storage_rocksdb: storage_rocksdb::Options,
}

#[derive(Debug)]
pub struct Worker {
    consensus: Consensus<
        partition::Command,
        PollSender<ConsensusCommand>,
        ReceiverStream<ConsensusMsg>,
        PollSender<ConsensusMsg>,
    >,
    processors: Vec<PartitionProcessor>,
    network: network_integration::Network,
    invoker:
        Invoker<ProtobufRawEntryCodec, RocksDBJournalReader, HashMap<String, EndpointMetadata>>,
}

impl Options {
    pub fn build(self) -> Worker {
        Worker::new(self)
    }
}

impl Worker {
    pub fn new(opts: Options) -> Self {
        let Options {
            channel_size,
            storage_rocksdb,
            ..
        } = opts;

        let storage = storage_rocksdb.build();
        let num_partition_processors = 10;
        let (raft_in_tx, raft_in_rx) = mpsc::channel(channel_size);
        let (ingress_tx, _ingress_rx) = mpsc::channel(channel_size);

        let network = network_integration::Network::new(
            raft_in_tx,
            ingress_tx,
            FixedPartitionTable::new(num_partition_processors),
        );

        let mut consensus = Consensus::new(
            ReceiverStream::new(raft_in_rx),
            network.create_consensus_sender(),
        );

        let network_handle = network.create_network_handle();

        let invoker = Invoker::new(RocksDBJournalReader, Default::default());

        let (command_senders, processors): (Vec<_>, Vec<_>) = (0..num_partition_processors)
            .map(|idx| {
                let proposal_sender = consensus.create_proposal_sender();
                let invoker_sender = invoker.create_sender();
                Self::create_partition_processor(
                    idx,
                    proposal_sender,
                    invoker_sender,
                    storage.clone(),
                    network_handle.clone(),
                )
            })
            .unzip();

        consensus.register_state_machines(command_senders);

        Self {
            consensus,
            processors,
            network,
            invoker,
        }
    }

    fn create_partition_processor(
        peer_id: PeerId,
        proposal_sender: ProposalSender<ConsensusMsg>,
        invoker_sender: UnboundedInvokerInputSender,
        storage: RocksDBStorage,
        network_handle: UnboundedNetworkHandle<shuffle::ShuffleInput, shuffle::ShuffleOutput>,
    ) -> ((PeerId, PollSender<ConsensusCommand>), PartitionProcessor) {
        let (command_tx, command_rx) = mpsc::channel(1);
        let processor = PartitionProcessor::new(
            peer_id,
            peer_id,
            ReceiverStream::new(command_rx),
            IdentitySender::new(peer_id, proposal_sender),
            invoker_sender,
            storage,
            network_handle,
        );

        ((peer_id, PollSender::new(command_tx)), processor)
    }

    pub async fn run(self, drain: drain::Watch) {
        let (shutdown_signal, shutdown_watch) = drain::channel();

        let mut invoker_handle = tokio::spawn(self.invoker.run(shutdown_watch.clone()));
        let mut network_handle = tokio::spawn(self.network.run(shutdown_watch));
        let mut consensus_handle = tokio::spawn(self.consensus.run());
        let mut processors_handles: FuturesUnordered<_> = self
            .processors
            .into_iter()
            .map(|partition_processor| tokio::spawn(partition_processor.run()))
            .collect();

        let shutdown = drain.signaled();

        tokio::select! {
            _ = shutdown => {
                debug!("Initiating shutdown of worker");

                // first we shut down the network which shuts down the consensus which shuts
                // down the partition processors transitively
                shutdown_signal.drain().await;

                // ignored because we are shutting down
                let _ = join!(network_handle, consensus_handle, processors_handles.collect::<Vec<_>>(), invoker_handle);

                debug!("Completed shutdown of worker");
            },
            invoker_result = &mut invoker_handle => {
                panic!("Invoker stopped running: {invoker_result:?}");
            },
            network_result = &mut network_handle => {
                panic!("Network stopped running: {network_result:?}");
            },
            consensus_result = &mut consensus_handle => {
                panic!("Consensus stopped running: {consensus_result:?}");
            },
            processor_result = processors_handles.next() => {
                panic!("One partition processor stopped running: {processor_result:?}");
            }
        }
    }
}
