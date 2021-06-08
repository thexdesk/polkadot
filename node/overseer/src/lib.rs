// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! # Overseer
//!
//! `overseer` implements the Overseer architecture described in the
//! [implementers-guide](https://w3f.github.io/parachain-implementers-guide/node/index.html).
//! For the motivations behind implementing the overseer itself you should
//! check out that guide, documentation in this crate will be mostly discussing
//! technical stuff.
//!
//! An `Overseer` is something that allows spawning/stopping and overseeing
//! asynchronous tasks as well as establishing a well-defined and easy to use
//! protocol that the tasks can use to communicate with each other. It is desired
//! that this protocol is the only way tasks communicate with each other, however
//! at this moment there are no foolproof guards against other ways of communication.
//!
//! The `Overseer` is instantiated with a pre-defined set of `Subsystems` that
//! share the same behavior from `Overseer`'s point of view.
//!
//! ```text
//!                              +-----------------------------+
//!                              |         Overseer            |
//!                              +-----------------------------+
//!
//!             ................|  Overseer "holds" these and uses |..............
//!             .                  them to (re)start things                      .
//!             .                                                                .
//!             .  +-------------------+                +---------------------+  .
//!             .  |   Subsystem1      |                |   Subsystem2        |  .
//!             .  +-------------------+                +---------------------+  .
//!             .           |                                       |            .
//!             ..................................................................
//!                         |                                       |
//!                       start()                                 start()
//!                         V                                       V
//!             ..................| Overseer "runs" these |.......................
//!             .  +--------------------+               +---------------------+  .
//!             .  | SubsystemInstance1 |               | SubsystemInstance2  |  .
//!             .  +--------------------+               +---------------------+  .
//!             ..................................................................
//! ```

// #![deny(unused_results)]
// unused dependencies can not work for test and examples at the same time
// yielding false positives
#![warn(missing_docs)]

use std::fmt::{self, Debug};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use std::collections::{hash_map, HashMap};
use std::iter::FromIterator;

use futures::channel::oneshot;
use futures::{
	select,
	future::BoxFuture,
	Future, FutureExt, StreamExt,
};

use polkadot_primitives::v1::{Block, BlockId,BlockNumber, Hash, ParachainHost};
use client::{BlockImportNotification, BlockchainEvents, FinalityNotification};
use sp_api::{ApiExt, ProvideRuntimeApi};

use polkadot_node_network_protocol::{
	v1 as protocol_v1,
};
use polkadot_node_subsystem::messages::{
	CandidateValidationMessage, CandidateBackingMessage,
	CandidateSelectionMessage, ChainApiMessage, StatementDistributionMessage,
	AvailabilityDistributionMessage, BitfieldSigningMessage, BitfieldDistributionMessage,
	ProvisionerMessage, RuntimeApiMessage,
	AvailabilityStoreMessage, NetworkBridgeMessage, CollationGenerationMessage,
	CollatorProtocolMessage, AvailabilityRecoveryMessage, ApprovalDistributionMessage,
	ApprovalVotingMessage, GossipSupportMessage,
	NetworkBridgeEvent,
};
pub use polkadot_node_subsystem::{
	OverseerSignal,
	errors::{SubsystemResult, SubsystemError,},
	ActiveLeavesUpdate, ActivatedLeaf, jaeger,
};

/// TODO legacy, to be deleted, left for easier integration
mod subsystems;
use self::subsystems::AllSubsystems;

mod metrics;
use self::metrics::Metrics;

use polkadot_node_subsystem_util::{metrics::{prometheus, Metrics as MetricsTrait}, Metronome};
use polkadot_overseer_gen::{
	TimeoutExt,
	SpawnNamed,
	Subsystem,
	SubsystemMeterReadouts,
	SubsystemMeters,
	SubsystemIncomingMessages,
	SubsystemInstance,
	SubsystemSender,
	SubsystemContext,
	overlord,
	MessagePacket,
	make_packet,
	SignalsReceived,
	FromOverseer,
	ToOverseer,
	MapSubsystem,
};

// Target for logs.
// const LOG_TARGET: &'static str = "parachain::overseer";

/// Whether a header supports parachain consensus or not.
pub trait HeadSupportsParachains {
	/// Return true if the given header supports parachain consensus. Otherwise, false.
	fn head_supports_parachains(&self, head: &Hash) -> bool;
}

impl<Client> HeadSupportsParachains for Arc<Client> where
	Client: ProvideRuntimeApi<Block>,
	Client::Api: ParachainHost<Block>,
{
	fn head_supports_parachains(&self, head: &Hash) -> bool {
		let id = BlockId::Hash(*head);
		self.runtime_api().has_api::<dyn ParachainHost<Block>>(&id).unwrap_or(false)
	}
}


/// A handler used to communicate with the [`Overseer`].
///
/// [`Overseer`]: struct.Overseer.html
#[derive(Clone)]
pub struct Handler(pub OverseerHandler);

impl Handler {
	/// Inform the `Overseer` that that some block was imported.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	pub async fn block_imported(&mut self, block: BlockInfo) {
		self.send_and_log_error(Event::BlockImported(block)).await
	}

	/// Send some message to one of the `Subsystem`s.
	#[tracing::instrument(level = "trace", skip(self, msg), fields(subsystem = LOG_TARGET))]
	pub async fn send_msg(&mut self, msg: impl Into<AllMessages>) {
		self.send_and_log_error(Event::MsgToSubsystem(msg.into())).await
	}

	/// Inform the `Overseer` that some block was finalized.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	pub async fn block_finalized(&mut self, block: BlockInfo) {
		self.send_and_log_error(Event::BlockFinalized(block)).await
	}

	/// Wait for a block with the given hash to be in the active-leaves set.
	///
	/// The response channel responds if the hash was activated and is closed if the hash was deactivated.
	/// Note that due the fact the overseer doesn't store the whole active-leaves set, only deltas,
	/// the response channel may never return if the hash was deactivated before this call.
	/// In this case, it's the caller's responsibility to ensure a timeout is set.
	#[tracing::instrument(level = "trace", skip(self, response_channel), fields(subsystem = LOG_TARGET))]
	pub async fn wait_for_activation(&mut self, hash: Hash, response_channel: oneshot::Sender<SubsystemResult<()>>) {
		self.send_and_log_error(Event::ExternalRequest(ExternalRequest::WaitForActivation {
				hash,
				response_channel
		})).await;
	}

	/// Tell `Overseer` to shutdown.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	pub async fn stop(&mut self) {
		self.send_and_log_error(Event::Stop).await;
	}

	async fn send_and_log_error(&mut self, event: Event) {
		if self.0.send(event).await.is_err() {
			tracing::info!(target: LOG_TARGET, "Failed to send an event to Overseer");
		}
	}
}

/// An event telling the `Overseer` on the particular block
/// that has been imported or finalized.
///
/// This structure exists solely for the purposes of decoupling
/// `Overseer` code from the client code and the necessity to call
/// `HeaderBackend::block_number_from_id()`.
#[derive(Debug, Clone)]
pub struct BlockInfo {
	/// hash of the block.
	pub hash: Hash,
	/// hash of the parent block.
	pub parent_hash: Hash,
	/// block's number.
	pub number: BlockNumber,
}

impl From<BlockImportNotification<Block>> for BlockInfo {
	fn from(n: BlockImportNotification<Block>) -> Self {
		BlockInfo {
			hash: n.hash,
			parent_hash: n.header.parent_hash,
			number: n.header.number,
		}
	}
}

impl From<FinalityNotification<Block>> for BlockInfo {
	fn from(n: FinalityNotification<Block>) -> Self {
		BlockInfo {
			hash: n.hash,
			parent_hash: n.header.parent_hash,
			number: n.header.number,
		}
	}
}

/// An event from outside the overseer scope, such
/// as the substrate framework or user interaction.
pub enum Event {
	/// A new block was imported.
	BlockImported(BlockInfo),
	/// A block was finalized with i.e. babe or another consensus algorithm.
	BlockFinalized(BlockInfo),
	/// An explicit message to the subsystem.
	MsgToSubsystem(AllMessages),
	/// An external request, see the inner type for details.
	ExternalRequest(ExternalRequest),
	/// Stop the overseer on i.e. a UNIX signal.
	Stop,
}

/// Some request from outer world.
pub enum ExternalRequest {
	/// Wait for the activation of a particular hash
	/// and be notified by means of the return channel.
	WaitForActivation {
		/// The relay parent for which activation to wait for.
		hash: Hash,
		/// Response channel to await on.
		response_channel: oneshot::Sender<SubsystemResult<()>>,
	},
}

/// Glues together the [`Overseer`] and `BlockchainEvents` by forwarding
/// import and finality notifications into the [`OverseerHandler`].
pub async fn forward_events<P: BlockchainEvents<Block>>(
	client: Arc<P>,
	mut handler: Handler,
) {
	let mut finality = client.finality_notification_stream();
	let mut imports = client.import_notification_stream();

	loop {
		select! {
			f = finality.next() => {
				match f {
					Some(block) => {
						handler.block_finalized(block.into()).await;
					}
					None => break,
				}
			},
			i = imports.next() => {
				match i {
					Some(block) => {
						handler.block_imported(block.into()).await;
					}
					None => break,
				}
			},
			complete => break,
		}
	}
}

/// The `Overseer` itself.
#[overlord(
	gen=AllMessages,
	event=Event,
	signal=OverseerSignal,
	error=SubsystemError,
	network=NetworkBridgeEvent<protocol_v1::ValidationProtocol>,
)]
pub struct Overseer<SupportsParachains> {

	#[subsystem(no_dispatch, CandidateValidationMessage)]
	candidate_validation: CandidateValidation,

	#[subsystem(no_dispatch, CandidateBackingMessage)]
	candidate_backing: CandidateBacking,

	#[subsystem(no_dispatch, CandidateSelectionMessage)]
	candidate_selection: CandidateSelection,

	#[subsystem(StatementDistributionMessage)]
	statement_distribution: StatementDistribution,

	#[subsystem(no_dispatch, AvailabilityDistributionMessage)]
	availability_distribution: AvailabilityDistribution,

	#[subsystem(no_dispatch, AvailabilityRecoveryMessage)]
	availability_recovery: AvailabilityRecovery,

	#[subsystem(blocking, no_dispatch, BitfieldSigningMessage)]
	bitfield_signing: BitfieldSigning,

	#[subsystem(BitfieldDistributionMessage)]
	bitfield_distribution: BitfieldDistribution,

	#[subsystem(no_dispatch, ProvisionerMessage)]
	provisioner: Provisioner,

	#[subsystem(no_dispatch, blocking, RuntimeApiMessage)]
	runtime_api: RuntimeApi,

	#[subsystem(no_dispatch, blocking, AvailabilityStoreMessage)]
	availability_store: AvailabilityStore,

	#[subsystem(no_dispatch, NetworkBridgeMessage)]
	network_bridge: NetworkBridge,

	#[subsystem(no_dispatch, blocking, ChainApiMessage)]
	chain_api: ChainApi,

	#[subsystem(no_dispatch, CollationGenerationMessage)]
	collation_generation: CollationGeneration,

	#[subsystem(no_dispatch, CollatorProtocolMessage)]
	collator_protocol: CollatorProtocol,

	#[subsystem(ApprovalDistributionMessage)]
	approval_distribution: ApprovalDistribution,

	#[subsystem(no_dispatch, ApprovalVotingMessage)]
	approval_voting: ApprovalVoting,

	#[subsystem(no_dispatch, GossipSupportMessage)]
	gossip_support: GossipSupport,

	/// External listeners waiting for a hash to be in the active-leave set.
	activation_external_listeners: HashMap<Hash, Vec<oneshot::Sender<SubsystemResult<()>>>>,

	/// Stores the [`jaeger::Span`] per active leaf.
	span_per_active_leaf: HashMap<Hash, Arc<jaeger::Span>>,

	/// A set of leaves that `Overseer` starts working with.
	///
	/// Drained at the beginning of `run` and never used again.
	pub leaves: Vec<(Hash, BlockNumber)>,

	/// The set of the "active leaves".
	pub active_leaves: HashMap<Hash, BlockNumber>,

	/// An implementation for checking whether a header supports parachain consensus.
	pub supports_parachains: SupportsParachains,

	/// Various Prometheus metrics.
	pub metrics: Metrics,
}

impl<S, SupportsParachains> Overseer<S, SupportsParachains>
where
	SupportsParachains: HeadSupportsParachains,
	S: SpawnNamed,
{
	/// Create a new instance of the `Overseer` with a fixed set of [`Subsystem`]s.
	///
	/// ```text
	///                  +------------------------------------+
	///                  |            Overseer                |
	///                  +------------------------------------+
	///                    /            |             |      \
	///      ................. subsystems...................................
	///      . +-----------+    +-----------+   +----------+   +---------+ .
	///      . |           |    |           |   |          |   |         | .
	///      . +-----------+    +-----------+   +----------+   +---------+ .
	///      ...............................................................
	///                              |
	///                        probably `spawn`
	///                            a `job`
	///                              |
	///                              V
	///                         +-----------+
	///                         |           |
	///                         +-----------+
	///
	/// ```
	///
	/// [`Subsystem`]: trait.Subsystem.html
	///
	/// # Example
	///
	/// The [`Subsystems`] may be any type as long as they implement an expected interface.
	/// Here, we create a mock validation subsystem and a few dummy ones and start the `Overseer` with them.
	/// For the sake of simplicity the termination of the example is done with a timeout.
	/// ```
	/// # use std::time::Duration;
	/// # use futures::{executor, pin_mut, select, FutureExt};
	/// # use futures_timer::Delay;
	/// # use polkadot_overseer::{Overseer, HeadSupportsParachains, AllSubsystems};
	/// # use polkadot_primitives::v1::Hash;
	/// # use polkadot_node_subsystem::{
	/// #     Subsystem, DummySubsystem, SpawnedSubsystem, SubsystemContext,
	/// #     messages::CandidateValidationMessage,
	/// # };
	///
	/// struct ValidationSubsystem;
	///
	/// impl<C> Subsystem<C> for ValidationSubsystem
	///     where C: SubsystemContext<Message=CandidateValidationMessage>
	/// {
	///     fn start(
	///         self,
	///         mut ctx: C,
	///     ) -> SpawnedSubsystem {
	///         SpawnedSubsystem {
	///             name: "validation-subsystem",
	///             future: Box::pin(async move {
	///                 loop {
	///                     Delay::new(Duration::from_secs(1)).await;
	///                 }
	///             }),
	///         }
	///     }
	/// }
	///
	/// # fn main() { executor::block_on(async move {
	///
	/// struct AlwaysSupportsParachains;
	/// impl HeadSupportsParachains for AlwaysSupportsParachains {
	///      fn head_supports_parachains(&self, _head: &Hash) -> bool { true }
	/// }
	/// let spawner = sp_core::testing::TaskExecutor::new();
	/// let all_subsystems = AllSubsystems::<()>::dummy().replace_candidate_validation(ValidationSubsystem);
	/// let (overseer, _handler) = Overseer::new(
	///     vec![],
	///     all_subsystems,
	///     None,
	///     AlwaysSupportsParachains,
	///     spawner,
	/// ).unwrap();
	///
	/// let timer = Delay::new(Duration::from_millis(50)).fuse();
	///
	/// let overseer_fut = overseer.run().fuse();
	/// pin_mut!(timer);
	/// pin_mut!(overseer_fut);
	///
	/// select! {
	///     _ = overseer_fut => (),
	///     _ = timer => (),
	/// }
	/// #
	/// # }); }
	/// ```
	pub fn new<CV, CB, CS, SD, AD, AR, BS, BD, P, RA, AS, NB, CA, CG, CP, ApD, ApV, GS>(
		leaves: impl IntoIterator<Item = BlockInfo>,
		all_subsystems: AllSubsystems<CV, CB, CS, SD, AD, AR, BS, BD, P, RA, AS, NB, CA, CG, CP, ApD, ApV, GS>,
		prometheus_registry: Option<&prometheus::Registry>,
		supports_parachains: SupportsParachains,
		s: S,
	) -> SubsystemResult<(Self, Handler)>
	where
		CV: Subsystem<OverseerSubsystemContext<CandidateValidationMessage>, SubsystemError> + Send,
		CB: Subsystem<OverseerSubsystemContext<CandidateBackingMessage>, SubsystemError> + Send,
		CS: Subsystem<OverseerSubsystemContext<CandidateSelectionMessage>, SubsystemError> + Send,
		SD: Subsystem<OverseerSubsystemContext<StatementDistributionMessage>, SubsystemError> + Send,
		AD: Subsystem<OverseerSubsystemContext<AvailabilityDistributionMessage>, SubsystemError> + Send,
		AR: Subsystem<OverseerSubsystemContext<AvailabilityRecoveryMessage>, SubsystemError> + Send,
		BS: Subsystem<OverseerSubsystemContext<BitfieldSigningMessage>, SubsystemError> + Send,
		BD: Subsystem<OverseerSubsystemContext<BitfieldDistributionMessage>, SubsystemError> + Send,
		P: Subsystem<OverseerSubsystemContext<ProvisionerMessage>, SubsystemError> + Send,
		RA: Subsystem<OverseerSubsystemContext<RuntimeApiMessage>, SubsystemError> + Send,
		AS: Subsystem<OverseerSubsystemContext<AvailabilityStoreMessage>, SubsystemError> + Send,
		NB: Subsystem<OverseerSubsystemContext<NetworkBridgeMessage>, SubsystemError> + Send,
		CA: Subsystem<OverseerSubsystemContext<ChainApiMessage>, SubsystemError> + Send,
		CG: Subsystem<OverseerSubsystemContext<CollationGenerationMessage>, SubsystemError> + Send,
		CP: Subsystem<OverseerSubsystemContext<CollatorProtocolMessage>, SubsystemError> + Send,
		ApD: Subsystem<OverseerSubsystemContext<ApprovalDistributionMessage>, SubsystemError> + Send,
		ApV: Subsystem<OverseerSubsystemContext<ApprovalVotingMessage>, SubsystemError> + Send,
		GS: Subsystem<OverseerSubsystemContext<GossipSupportMessage>, SubsystemError> + Send,
		S: SpawnNamed,
	{
		let metrics: Metrics = <Metrics as MetricsTrait>::register(prometheus_registry)?;

		let (mut overseer, handler) = Self::builder()
			.candidate_validation(all_subsystems.candidate_validation)
			.candidate_backing(all_subsystems.candidate_backing)
			.candidate_selection(all_subsystems.candidate_selection)
			.statement_distribution(all_subsystems.statement_distribution)
			.availability_distribution(all_subsystems.availability_distribution)
			.availability_recovery(all_subsystems.availability_recovery)
			.bitfield_signing(all_subsystems.bitfield_signing)
			.bitfield_distribution(all_subsystems.bitfield_distribution)
			.provisioner(all_subsystems.provisioner)
			.runtime_api(all_subsystems.runtime_api)
			.availability_store(all_subsystems.availability_store)
			.network_bridge(all_subsystems.network_bridge)
			.chain_api(all_subsystems.chain_api)
			.collation_generation(all_subsystems.collation_generation)
			.collator_protocol(all_subsystems.collator_protocol)
			.approval_distribution(all_subsystems.approval_distribution)
			.approval_voting(all_subsystems.approval_voting)
			.gossip_support(all_subsystems.gossip_support)
			.leaves(Vec::from_iter(
				leaves.into_iter().map(|BlockInfo { hash, parent_hash: _, number }| (hash, number))
			))
			.active_leaves(Default::default())
			.span_per_active_leaf(Default::default())
			.activation_external_listeners(Default::default())
			.metrics(metrics.clone())
			.spawner(s)
			.build()?;

		// spawn the metrics metronome task
		{
			struct ExtractNameAndMeters;

			impl<'a, T: 'a> MapSubsystem<&'a OverseenSubsystem<T>> for ExtractNameAndMeters {
				type Output = Option<(&'static str, SubsystemMeters)>;

				fn map_subsystem(&self, subsystem: &'a OverseenSubsystem<T>) -> Self::Output {
					subsystem.instance.as_ref().map(|instance| {
						(
							instance.name,
							instance.meters.clone(),
						)
					})
				}
			}
			let subsystem_meters = overseer.map_subsystems(ExtractNameAndMeters);

			let metronome_metrics = metrics.clone();
			let metronome = Metronome::new(std::time::Duration::from_millis(950))
				.for_each(move |_| {

					// We combine the amount of messages from subsystems to the overseer
					// as well as the amount of messages from external sources to the overseer
					// into one `to_overseer` value.
					metronome_metrics.channel_fill_level_snapshot(
						subsystem_meters.iter()
							.cloned()
							.filter_map(|x| x)
							.map(|(name, ref meters)| (name, meters.read()))
					);

					async move {
						()
					}
				});
			overseer.spawner().spawn("metrics_metronome", Box::pin(metronome));
		}

		Ok((overseer, Handler(handler)))
	}

	// Stop the overseer.
	async fn stop(mut self) {
		let _ = self.wait_terminate(OverseerSignal::Conclude, ::std::time::Duration::from_secs(1_u64)).await;
	}

	/// Run the `Overseer`.
	#[tracing::instrument(skip(self), fields(subsystem = LOG_TARGET))]
	pub async fn run(mut self) -> SubsystemResult<()> {
		let mut update = ActiveLeavesUpdate::default();

		for (hash, number) in std::mem::take(&mut self.leaves) {
			let _ = self.active_leaves.insert(hash, number);
			if let Some(span) = self.on_head_activated(&hash, None) {
				update.activated.push(ActivatedLeaf {
					hash,
					number,
					span,
				});
			}
		}

		if !update.is_empty() {
			self.broadcast_signal(OverseerSignal::ActiveLeaves(update)).await?;
		}

		loop {
			select! {
				msg = self.events_rx.next().fuse() => {
					let msg = if let Some(msg) = msg {
						msg
					} else {
						continue
					};

					match msg {
						Event::MsgToSubsystem(msg) => {
							self.route_message(msg.into()).await?;
						}
						Event::Stop => {
							self.stop().await;
							return Ok(());
						}
						Event::BlockImported(block) => {
							self.block_imported(block).await?;
						}
						Event::BlockFinalized(block) => {
							self.block_finalized(block).await?;
						}
						Event::ExternalRequest(request) => {
							self.handle_external_request(request);
						}
					}
				},
				msg = self.to_overseer_rx.next() => {
					let msg = match msg {
						Some(m) => m,
						None => {
							// This is a fused stream so we will shut down after receiving all
							// shutdown notifications.
							continue
						}
					};

					match msg {
						ToOverseer::SpawnJob { name, s } => {
							self.spawn_job(name, s);
						}
						ToOverseer::SpawnBlockingJob { name, s } => {
							self.spawn_blocking_job(name, s);
						}
					}
				},
				res = self.running_subsystems.next().fuse() => {
					let finished = if let Some(finished) = res {
						finished
					} else {
						continue
					};

					tracing::error!(target: LOG_TARGET, subsystem = ?finished, "subsystem finished unexpectedly");
					self.stop().await;
					return Ok(finished?);
				},
			}
		}
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	async fn block_imported(&mut self, block: BlockInfo) -> SubsystemResult<()> {
		match self.active_leaves.entry(block.hash) {
			hash_map::Entry::Vacant(entry) => entry.insert(block.number),
			hash_map::Entry::Occupied(entry) => {
				debug_assert_eq!(*entry.get(), block.number);
				return Ok(());
			}
		};

		let mut update = match self.on_head_activated(&block.hash, Some(block.parent_hash)) {
			Some(span) => ActiveLeavesUpdate::start_work(ActivatedLeaf {
				hash: block.hash,
				number: block.number,
				span
			}),
			None => ActiveLeavesUpdate::default(),
		};

		if let Some(number) = self.active_leaves.remove(&block.parent_hash) {
			debug_assert_eq!(block.number.saturating_sub(1), number);
			update.deactivated.push(block.parent_hash);
			self.on_head_deactivated(&block.parent_hash);
		}

		self.clean_up_external_listeners();

		if !update.is_empty() {
			self.broadcast_signal(OverseerSignal::ActiveLeaves(update)).await?;
		}
		Ok(())
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	async fn block_finalized(&mut self, block: BlockInfo) -> SubsystemResult<()> {
		let mut update = ActiveLeavesUpdate::default();

		self.active_leaves.retain(|h, n| {
			if *n <= block.number {
				update.deactivated.push(*h);
				false
			} else {
				true
			}
		});

		for deactivated in &update.deactivated {
			self.on_head_deactivated(deactivated)
		}

		self.broadcast_signal(OverseerSignal::BlockFinalized(block.hash, block.number)).await?;

		// If there are no leaves being deactivated, we don't need to send an update.
		//
		// Our peers will be informed about our finalized block the next time we activating/deactivating some leaf.
		if !update.is_empty() {
			self.broadcast_signal(OverseerSignal::ActiveLeaves(update)).await?;
		}

		Ok(())
	}

	/// Handles a header activation. If the header's state doesn't support the parachains API,
	/// this returns `None`.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	fn on_head_activated(&mut self, hash: &Hash, parent_hash: Option<Hash>)
		-> Option<Arc<jaeger::Span>>
	{
		if !self.supports_parachains.head_supports_parachains(hash) {
			return None;
		}

		self.metrics.on_head_activated();
		if let Some(listeners) = self.activation_external_listeners.remove(hash) {
			for listener in listeners {
				// it's fine if the listener is no longer interested
				let _ = listener.send(Ok(()));
			}
		}

		let mut span = jaeger::Span::new(*hash, "leaf-activated");

		if let Some(parent_span) = parent_hash.and_then(|h| self.span_per_active_leaf.get(&h)) {
			span.add_follows_from(&*parent_span);
		}

		let span = Arc::new(span);
		self.span_per_active_leaf.insert(*hash, span.clone());
		Some(span)
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	fn on_head_deactivated(&mut self, hash: &Hash) {
		self.metrics.on_head_deactivated();
		self.activation_external_listeners.remove(hash);
		self.span_per_active_leaf.remove(hash);
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	fn clean_up_external_listeners(&mut self) {
		self.activation_external_listeners.retain(|_, v| {
			// remove dead listeners
			v.retain(|c| !c.is_canceled());
			!v.is_empty()
		})
	}

	#[tracing::instrument(level = "trace", skip(self, request), fields(subsystem = LOG_TARGET))]
	fn handle_external_request(&mut self, request: ExternalRequest) {
		match request {
			ExternalRequest::WaitForActivation { hash, response_channel } => {
				if self.active_leaves.get(&hash).is_some() {
					// it's fine if the listener is no longer interested
					let _ = response_channel.send(Ok(()));
				} else {
					self.activation_external_listeners.entry(hash).or_default().push(response_channel);
				}
			}
		}
	}

	fn spawn_job(&mut self, name: &'static str, j: BoxFuture<'static, ()>) {
		self.spawner.spawn(name, j);
	}

	fn spawn_blocking_job(&mut self, name: &'static str, j: BoxFuture<'static, ()>) {
		self.spawner.spawn_blocking(name, j);
	}
}



#[cfg(test)]
mod tests;
