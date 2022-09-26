// This file is part of Substrate.

// Copyright (C) 2018-2022 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Substrate transaction pool implementation.

#![recursion_limit = "256"]
#![warn(missing_docs)]
#![warn(unused_extern_crates)]

mod api;
pub mod error;
mod graph;
mod metrics;
mod revalidation;
#[cfg(test)]
mod tests;

pub use crate::api::FullChainApi;
use futures::{
	channel::oneshot,
	future::{self, ready},
	prelude::*,
};
pub use graph::{
	base_pool::Limit as PoolLimit, ChainApi, Options, Pool, Transaction, ValidatedTransaction,
};
use parking_lot::Mutex;
use std::{
	collections::{HashMap, HashSet},
	pin::Pin,
	sync::Arc,
};

use graph::{ExtrinsicHash, IsValidator};
use sc_transaction_pool_api::{
	error::Error as TxPoolError, ChainEvent, ImportNotificationStream, MaintainedTransactionPool,
	PoolFuture, PoolStatus, ReadyTransactions, TransactionFor, TransactionPool, TransactionSource,
	TransactionStatusStreamFor, TxHash,
};
use sp_core::traits::SpawnEssentialNamed;
use sp_runtime::{
	generic::BlockId,
	traits::{AtLeast32Bit, Block as BlockT, Extrinsic, Header as HeaderT, NumberFor, Zero},
};
use std::time::Instant;

use crate::metrics::MetricsLink as PrometheusMetrics;
use prometheus_endpoint::Registry as PrometheusRegistry;

use sp_blockchain::TreeRoute;

type BoxedReadyIterator<Hash, Data> =
	Box<dyn ReadyTransactions<Item = Arc<graph::base_pool::Transaction<Hash, Data>>> + Send>;

type ReadyIteratorFor<PoolApi> =
	BoxedReadyIterator<graph::ExtrinsicHash<PoolApi>, graph::ExtrinsicFor<PoolApi>>;

type PolledIterator<PoolApi> = Pin<Box<dyn Future<Output = ReadyIteratorFor<PoolApi>> + Send>>;

/// A transaction pool for a full node.
pub type FullPool<Block, Client> = BasicPool<FullChainApi<Client, Block>, Block>;

/// Basic implementation of transaction pool that can be customized by providing PoolApi.
pub struct BasicPool<PoolApi, Block>
where
	Block: BlockT,
	PoolApi: graph::ChainApi<Block = Block>,
{
	pool: Arc<graph::Pool<PoolApi>>,
	api: Arc<PoolApi>,
	revalidation_strategy: Arc<Mutex<RevalidationStrategy<NumberFor<Block>>>>,
	revalidation_queue: Arc<revalidation::RevalidationQueue<PoolApi>>,
	ready_poll: Arc<Mutex<ReadyPoll<ReadyIteratorFor<PoolApi>, Block>>>,
	metrics: PrometheusMetrics,

	enactment_helper: Arc<Mutex<EnactmentHelper<Block, PoolApi>>>,
}

struct ReadyPoll<T, Block: BlockT> {
	updated_at: NumberFor<Block>,
	pollers: Vec<(NumberFor<Block>, oneshot::Sender<T>)>,
}

impl<T, Block: BlockT> Default for ReadyPoll<T, Block> {
	fn default() -> Self {
		Self { updated_at: NumberFor::<Block>::zero(), pollers: Default::default() }
	}
}

impl<T, Block: BlockT> ReadyPoll<T, Block> {
	fn new(best_block_number: NumberFor<Block>) -> Self {
		Self { updated_at: best_block_number, pollers: Default::default() }
	}

	fn trigger(&mut self, number: NumberFor<Block>, iterator_factory: impl Fn() -> T) {
		self.updated_at = number;

		let mut idx = 0;
		while idx < self.pollers.len() {
			if self.pollers[idx].0 <= number {
				let poller_sender = self.pollers.swap_remove(idx);
				log::debug!(target: "txpool", "Sending ready signal at block {}", number);
				let _ = poller_sender.1.send(iterator_factory());
			} else {
				idx += 1;
			}
		}
	}

	fn add(&mut self, number: NumberFor<Block>) -> oneshot::Receiver<T> {
		let (sender, receiver) = oneshot::channel();
		self.pollers.push((number, sender));
		receiver
	}

	fn updated_at(&self) -> NumberFor<Block> {
		self.updated_at
	}
}

impl<PoolApi, Block> parity_util_mem::MallocSizeOf for BasicPool<PoolApi, Block>
where
	PoolApi: graph::ChainApi<Block = Block>,
	Block: BlockT,
{
	fn size_of(&self, ops: &mut parity_util_mem::MallocSizeOfOps) -> usize {
		// other entries insignificant or non-primary references
		self.pool.size_of(ops)
	}
}

/// Type of revalidation.
pub enum RevalidationType {
	/// Light revalidation type.
	///
	/// During maintenance, transaction pool makes periodic revalidation
	/// of all transactions depending on number of blocks or time passed.
	/// Also this kind of revalidation does not resubmit transactions from
	/// retracted blocks, since it is too expensive.
	Light,

	/// Full revalidation type.
	///
	/// During maintenance, transaction pool revalidates some fixed amount of
	/// transactions from the pool of valid transactions.
	Full,
}

impl<PoolApi, Block> BasicPool<PoolApi, Block>
where
	Block: BlockT,
	PoolApi: graph::ChainApi<Block = Block> + 'static,
{
	/// Create new basic transaction pool with provided api, for tests.
	pub fn new_test(pool_api: Arc<PoolApi>) -> (Self, Pin<Box<dyn Future<Output = ()> + Send>>) {
		let pool = Arc::new(graph::Pool::new(Default::default(), true.into(), pool_api.clone()));
		let (revalidation_queue, background_task) =
			revalidation::RevalidationQueue::new_background(pool_api.clone(), pool.clone());
		(
			Self {
				api: pool_api.clone(),
				pool,
				revalidation_queue: Arc::new(revalidation_queue),
				revalidation_strategy: Arc::new(Mutex::new(RevalidationStrategy::Always)),
				ready_poll: Default::default(),
				metrics: Default::default(),
				enactment_helper: Arc::new(Mutex::new(EnactmentHelper::new(pool_api.clone()))),
			},
			background_task,
		)
	}

	/// Create new basic transaction pool with provided api and custom
	/// revalidation type.
	pub fn with_revalidation_type(
		options: graph::Options,
		is_validator: IsValidator,
		pool_api: Arc<PoolApi>,
		prometheus: Option<&PrometheusRegistry>,
		revalidation_type: RevalidationType,
		spawner: impl SpawnEssentialNamed,
		best_block_number: NumberFor<Block>,
	) -> Self {
		let pool = Arc::new(graph::Pool::new(options, is_validator, pool_api.clone()));
		let (revalidation_queue, background_task) = match revalidation_type {
			RevalidationType::Light =>
				(revalidation::RevalidationQueue::new(pool_api.clone(), pool.clone()), None),
			RevalidationType::Full => {
				let (queue, background) =
					revalidation::RevalidationQueue::new_background(pool_api.clone(), pool.clone());
				(queue, Some(background))
			},
		};

		if let Some(background_task) = background_task {
			spawner.spawn_essential("txpool-background", Some("transaction-pool"), background_task);
		}

		Self {
			api: pool_api.clone(),
			pool,
			revalidation_queue: Arc::new(revalidation_queue),
			revalidation_strategy: Arc::new(Mutex::new(match revalidation_type {
				RevalidationType::Light =>
					RevalidationStrategy::Light(RevalidationStatus::NotScheduled),
				RevalidationType::Full => RevalidationStrategy::Always,
			})),
			ready_poll: Arc::new(Mutex::new(ReadyPoll::new(best_block_number))),
			metrics: PrometheusMetrics::new(prometheus),
			enactment_helper: Arc::new(Mutex::new(EnactmentHelper::new(pool_api))),
		}
	}

	/// Gets shared reference to the underlying pool.
	pub fn pool(&self) -> &Arc<graph::Pool<PoolApi>> {
		&self.pool
	}

	/// Get access to the underlying api
	pub fn api(&self) -> &PoolApi {
		&self.api
	}
}

impl<PoolApi, Block> TransactionPool for BasicPool<PoolApi, Block>
where
	Block: BlockT,
	PoolApi: 'static + graph::ChainApi<Block = Block>,
{
	type Block = PoolApi::Block;
	type Hash = graph::ExtrinsicHash<PoolApi>;
	type InPoolTransaction = graph::base_pool::Transaction<TxHash<Self>, TransactionFor<Self>>;
	type Error = PoolApi::Error;

	fn submit_at(
		&self,
		at: &BlockId<Self::Block>,
		source: TransactionSource,
		xts: Vec<TransactionFor<Self>>,
	) -> PoolFuture<Vec<Result<TxHash<Self>, Self::Error>>, Self::Error> {
		let pool = self.pool.clone();
		let at = *at;

		self.metrics
			.report(|metrics| metrics.submitted_transactions.inc_by(xts.len() as u64));

		async move { pool.submit_at(&at, source, xts).await }.boxed()
	}

	fn submit_one(
		&self,
		at: &BlockId<Self::Block>,
		source: TransactionSource,
		xt: TransactionFor<Self>,
	) -> PoolFuture<TxHash<Self>, Self::Error> {
		let pool = self.pool.clone();
		let at = *at;

		self.metrics.report(|metrics| metrics.submitted_transactions.inc());

		async move { pool.submit_one(&at, source, xt).await }.boxed()
	}

	fn submit_and_watch(
		&self,
		at: &BlockId<Self::Block>,
		source: TransactionSource,
		xt: TransactionFor<Self>,
	) -> PoolFuture<Pin<Box<TransactionStatusStreamFor<Self>>>, Self::Error> {
		let at = *at;
		let pool = self.pool.clone();

		self.metrics.report(|metrics| metrics.submitted_transactions.inc());

		async move {
			let watcher = pool.submit_and_watch(&at, source, xt).await?;

			Ok(watcher.into_stream().boxed())
		}
		.boxed()
	}

	fn remove_invalid(&self, hashes: &[TxHash<Self>]) -> Vec<Arc<Self::InPoolTransaction>> {
		let removed = self.pool.validated_pool().remove_invalid(hashes);
		self.metrics
			.report(|metrics| metrics.validations_invalid.inc_by(removed.len() as u64));
		removed
	}

	fn status(&self) -> PoolStatus {
		self.pool.validated_pool().status()
	}

	fn import_notification_stream(&self) -> ImportNotificationStream<TxHash<Self>> {
		self.pool.validated_pool().import_notification_stream()
	}

	fn hash_of(&self, xt: &TransactionFor<Self>) -> TxHash<Self> {
		self.pool.hash_of(xt)
	}

	fn on_broadcasted(&self, propagations: HashMap<TxHash<Self>, Vec<String>>) {
		self.pool.validated_pool().on_broadcasted(propagations)
	}

	fn ready_transaction(&self, hash: &TxHash<Self>) -> Option<Arc<Self::InPoolTransaction>> {
		self.pool.validated_pool().ready_by_hash(hash)
	}

	fn ready_at(&self, at: NumberFor<Self::Block>) -> PolledIterator<PoolApi> {
		let status = self.status();
		// If there are no transactions in the pool, it is fine to return early.
		//
		// There could be transaction being added because of some re-org happening at the relevant
		// block, but this is relative unlikely.
		if status.ready == 0 && status.future == 0 {
			return async { Box::new(std::iter::empty()) as Box<_> }.boxed()
		}

		if self.ready_poll.lock().updated_at() >= at {
			log::trace!(target: "txpool", "Transaction pool already processed block  #{}", at);
			let iterator: ReadyIteratorFor<PoolApi> = Box::new(self.pool.validated_pool().ready());
			return async move { iterator }.boxed()
		}

		self.ready_poll
			.lock()
			.add(at)
			.map(|received| {
				received.unwrap_or_else(|e| {
					log::warn!("Error receiving pending set: {:?}", e);
					Box::new(std::iter::empty())
				})
			})
			.boxed()
	}

	fn ready(&self) -> ReadyIteratorFor<PoolApi> {
		Box::new(self.pool.validated_pool().ready())
	}
}

impl<Block, Client> FullPool<Block, Client>
where
	Block: BlockT,
	Client: sp_api::ProvideRuntimeApi<Block>
		+ sc_client_api::BlockBackend<Block>
		+ sc_client_api::blockchain::HeaderBackend<Block>
		+ sp_runtime::traits::BlockIdTo<Block>
		+ sc_client_api::ExecutorProvider<Block>
		+ sc_client_api::UsageProvider<Block>
		+ sp_blockchain::HeaderMetadata<Block, Error = sp_blockchain::Error>
		+ Send
		+ Sync
		+ 'static,
	Client::Api: sp_transaction_pool::runtime_api::TaggedTransactionQueue<Block>,
{
	/// Create new basic transaction pool for a full node with the provided api.
	pub fn new_full(
		options: graph::Options,
		is_validator: IsValidator,
		prometheus: Option<&PrometheusRegistry>,
		spawner: impl SpawnEssentialNamed,
		client: Arc<Client>,
	) -> Arc<Self> {
		let pool_api = Arc::new(FullChainApi::new(client.clone(), prometheus, &spawner));
		let pool = Arc::new(Self::with_revalidation_type(
			options,
			is_validator,
			pool_api,
			prometheus,
			RevalidationType::Full,
			spawner,
			client.usage_info().chain.best_number,
		));

		// make transaction pool available for off-chain runtime calls.
		client.execution_extensions().register_transaction_pool(&pool);

		pool
	}
}

impl<Block, Client> sc_transaction_pool_api::LocalTransactionPool
	for BasicPool<FullChainApi<Client, Block>, Block>
where
	Block: BlockT,
	Client: sp_api::ProvideRuntimeApi<Block>
		+ sc_client_api::BlockBackend<Block>
		+ sc_client_api::blockchain::HeaderBackend<Block>
		+ sp_runtime::traits::BlockIdTo<Block>
		+ sp_blockchain::HeaderMetadata<Block, Error = sp_blockchain::Error>,
	Client: Send + Sync + 'static,
	Client::Api: sp_transaction_pool::runtime_api::TaggedTransactionQueue<Block>,
{
	type Block = Block;
	type Hash = graph::ExtrinsicHash<FullChainApi<Client, Block>>;
	type Error = <FullChainApi<Client, Block> as graph::ChainApi>::Error;

	fn submit_local(
		&self,
		at: &BlockId<Self::Block>,
		xt: sc_transaction_pool_api::LocalTransactionFor<Self>,
	) -> Result<Self::Hash, Self::Error> {
		use sp_runtime::{
			traits::SaturatedConversion, transaction_validity::TransactionValidityError,
		};

		let validity = self
			.api
			.validate_transaction_blocking(at, TransactionSource::Local, xt.clone())?
			.map_err(|e| {
				Self::Error::Pool(match e {
					TransactionValidityError::Invalid(i) => TxPoolError::InvalidTransaction(i),
					TransactionValidityError::Unknown(u) => TxPoolError::UnknownTransaction(u),
				})
			})?;

		let (hash, bytes) = self.pool.validated_pool().api().hash_and_length(&xt);
		let block_number = self
			.api
			.block_id_to_number(at)?
			.ok_or_else(|| error::Error::BlockIdConversion(format!("{:?}", at)))?;

		let validated = ValidatedTransaction::valid_at(
			block_number.saturated_into::<u64>(),
			hash,
			TransactionSource::Local,
			xt,
			bytes,
			validity,
		);

		self.pool.validated_pool().submit(vec![validated]).remove(0)
	}
}

#[cfg_attr(test, derive(Debug))]
enum RevalidationStatus<N> {
	/// The revalidation has never been completed.
	NotScheduled,
	/// The revalidation is scheduled.
	Scheduled(Option<Instant>, Option<N>),
	/// The revalidation is in progress.
	InProgress,
}

enum RevalidationStrategy<N> {
	Always,
	Light(RevalidationStatus<N>),
}

struct RevalidationAction {
	revalidate: bool,
	resubmit: bool,
}

impl<N: Clone + Copy + AtLeast32Bit> RevalidationStrategy<N> {
	pub fn clear(&mut self) {
		if let Self::Light(status) = self {
			status.clear()
		}
	}

	pub fn next(
		&mut self,
		block: N,
		revalidate_time_period: Option<std::time::Duration>,
		revalidate_block_period: Option<N>,
	) -> RevalidationAction {
		match self {
			Self::Light(status) => RevalidationAction {
				revalidate: status.next_required(
					block,
					revalidate_time_period,
					revalidate_block_period,
				),
				resubmit: false,
			},
			Self::Always => RevalidationAction { revalidate: true, resubmit: true },
		}
	}
}

impl<N: Clone + Copy + AtLeast32Bit> RevalidationStatus<N> {
	/// Called when revalidation is completed.
	pub fn clear(&mut self) {
		*self = Self::NotScheduled;
	}

	/// Returns true if revalidation is required.
	pub fn next_required(
		&mut self,
		block: N,
		revalidate_time_period: Option<std::time::Duration>,
		revalidate_block_period: Option<N>,
	) -> bool {
		match *self {
			Self::NotScheduled => {
				*self = Self::Scheduled(
					revalidate_time_period.map(|period| Instant::now() + period),
					revalidate_block_period.map(|period| block + period),
				);
				false
			},
			Self::Scheduled(revalidate_at_time, revalidate_at_block) => {
				let is_required =
					revalidate_at_time.map(|at| Instant::now() >= at).unwrap_or(false) ||
						revalidate_at_block.map(|at| block >= at).unwrap_or(false);
				if is_required {
					*self = Self::InProgress;
				}
				is_required
			},
			Self::InProgress => false,
		}
	}
}

/// Prune the known txs for the given block.
async fn prune_known_txs_for_block<Block: BlockT, Api: graph::ChainApi<Block = Block>>(
	block_id: BlockId<Block>,
	api: &Api,
	pool: &graph::Pool<Api>,
) -> Vec<ExtrinsicHash<Api>> {
	let extrinsics = api
		.block_body(&block_id)
		.await
		.unwrap_or_else(|e| {
			log::warn!("Prune known transactions: error request: {}", e);
			None
		})
		.unwrap_or_default();

	let hashes = extrinsics.iter().map(|tx| pool.hash_of(tx)).collect::<Vec<_>>();

	log::trace!(target: "txpool", "Pruning transactions: {:?}", hashes);

	let header = match api.block_header(&block_id) {
		Ok(Some(h)) => h,
		Ok(None) => {
			log::debug!(target: "txpool", "Could not find header for {:?}.", block_id);
			return hashes
		},
		Err(e) => {
			log::debug!(target: "txpool", "Error retrieving header for {:?}: {}", block_id, e);
			return hashes
		},
	};

	if let Err(e) = pool.prune(&block_id, &BlockId::hash(*header.parent_hash()), &extrinsics).await
	{
		log::error!("Cannot prune known in the pool: {}", e);
	}

	hashes
}

impl<PoolApi, Block> BasicPool<PoolApi, Block>
where
	Block: BlockT,
	PoolApi: 'static + graph::ChainApi<Block = Block>,
{
	/// enactment_helper getter, intended for tests only
	pub fn enactment_helper(&self) -> Arc<Mutex<dyn EnactmentPolicy<Block>>> {
		self.enactment_helper.clone()
	}
}

impl<PoolApi, Block> BasicPool<PoolApi, Block>
where
	Block: BlockT,
	PoolApi: 'static + graph::ChainApi<Block = Block>,
{
	fn handle_enactment(
		&self,
		hash: Block::Hash,
		tree_route: Option<Arc<TreeRoute<Block>>>,
	) -> Pin<Box<dyn Future<Output = ()> + Send>> {
		log::trace!(target: "txpool", "handle_enactment hash:{hash:?} tree_route: {tree_route:?}");
		let pool = self.pool.clone();
		let api = self.api.clone();

		let (enacted, retracted) = if let Some(tree_route) = tree_route {
			(
				tree_route.enacted().iter().map(|block| block.hash).collect(),
				tree_route.retracted().iter().map(|block| block.hash).collect(),
			)
		} else {
			(vec![], vec![])
		};

		let id = BlockId::hash(hash);
		let block_number = match api.block_id_to_number(&id) {
			Ok(Some(number)) => number,
			_ => {
				log::trace!(
				target: "txpool",
				"Skipping chain event - no number for that block {:?}",
				id,
				);
				return Box::pin(ready(()))
			},
		};

		let next_action = self.revalidation_strategy.lock().next(
			block_number,
			Some(std::time::Duration::from_secs(60)),
			Some(20u32.into()),
		);
		let revalidation_strategy = self.revalidation_strategy.clone();
		let revalidation_queue = self.revalidation_queue.clone();
		let ready_poll = self.ready_poll.clone();
		let metrics = self.metrics.clone();

		async move {
			// We keep track of everything we prune so that later we won't add
			// transactions with those hashes from the retracted blocks.
			let mut pruned_log = HashSet::<ExtrinsicHash<PoolApi>>::new();

			// If there is a tree route, we use this to prune known tx based on the enacted
			// blocks. Before pruning enacted transactions, we inform the listeners about
			// retracted blocks and their transactions. This order is important, because
			// if we enact and retract the same transaction at the same time, we want to
			// send first the retract and than the prune event.
			for retracted_hash in retracted.iter() {
				// notify txs awaiting finality that it has been retracted
				pool.validated_pool().on_block_retracted(*retracted_hash);
			}

			future::join_all(
				enacted
					.iter()
					.map(|hash| prune_known_txs_for_block(BlockId::Hash(*hash), &*api, &*pool)),
			)
			.await
			.into_iter()
			.for_each(|enacted_log| {
				pruned_log.extend(enacted_log);
			});

			if enacted.iter().all(|h| *h != hash) {
				pruned_log.extend(prune_known_txs_for_block(id, &*api, &*pool).await);
			}

			metrics.report(|metrics| {
				metrics.block_transactions_pruned.inc_by(pruned_log.len() as u64)
			});

			// is Some(tree_route) == (enacted.len != 0 || retracted.len != 0)
			if let true = next_action.resubmit {
				let mut resubmit_transactions = Vec::new();

				for hash in retracted.iter() {
					let block_transactions = api
						.block_body(&BlockId::hash(*hash))
						.await
						.unwrap_or_else(|e| {
							log::warn!("Failed to fetch block body: {}", e);
							None
						})
						.unwrap_or_default()
						.into_iter()
						.filter(|tx| tx.is_signed().unwrap_or(true));

					let mut resubmitted_to_report = 0;

					resubmit_transactions.extend(block_transactions.into_iter().filter(|tx| {
						let tx_hash = pool.hash_of(tx);
						let contains = pruned_log.contains(&tx_hash);

						// need to count all transactions, not just filtered, here
						resubmitted_to_report += 1;

						if !contains {
							log::debug!(
							target: "txpool",
							"[{:?}]: Resubmitting from retracted block {:?}",
							tx_hash,
							hash,
							);
						}
						!contains
					}));

					metrics.report(|metrics| {
						metrics.block_transactions_resubmitted.inc_by(resubmitted_to_report)
					});
				}

				if let Err(e) = pool
					.resubmit_at(
						&id,
						// These transactions are coming from retracted blocks, we should
						// simply consider them external.
						TransactionSource::External,
						resubmit_transactions,
					)
					.await
				{
					log::debug!(
					target: "txpool",
					"[{:?}] Error re-submitting transactions: {}",
					id,
					e,
					)
				}
			}

			let extra_pool = pool.clone();
			// After #5200 lands, this arguably might be moved to the
			// handler of "all blocks notification".
			ready_poll
				.lock()
				.trigger(block_number, move || Box::new(extra_pool.validated_pool().ready()));

			if next_action.revalidate {
				let hashes = pool.validated_pool().ready().map(|tx| tx.hash).collect();
				revalidation_queue.revalidate_later(block_number, hashes).await;

				revalidation_strategy.lock().clear();
			}
		}
		.boxed()
	}
}

impl<PoolApi, Block> MaintainedTransactionPool for BasicPool<PoolApi, Block>
where
	Block: BlockT,
	PoolApi: 'static + graph::ChainApi<Block = Block>,
{
	fn maintain(&self, event: ChainEvent<Self::Block>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
		let (proceed, tree_route) = self.enactment_helper.lock().should_handle_enactment(&event);

		let hash = match event {
			ChainEvent::NewBestBlock { hash, .. } | ChainEvent::Finalized { hash, .. } => hash,
		};

		let handle_enactment = if proceed {
			self.handle_enactment(hash, tree_route.clone())
		} else {
			Box::pin(ready(()))
		};

		match event {
			ChainEvent::NewBestBlock { .. } => handle_enactment,
			ChainEvent::Finalized { hash, .. } => {
				let pool = self.pool.clone();

				async move {
					handle_enactment.await;
					let enacted = if let Some(tree_route) = tree_route {
						tree_route.enacted().to_vec()
					} else {
						vec![]
					};

					for hash in enacted.iter().map(|b| b.hash).chain(std::iter::once(hash)) {
						if let Err(e) = pool.validated_pool().on_block_finalized(hash).await {
							log::warn!(
							target: "txpool",
							"Error [{}] occurred while attempting to notify watchers of finalization {}",
							e, hash
							)
						}
					}
				}
				.boxed()
			},
		}
	}
}

/// Trait for deciding if core part of maintenance procedure shall be executed.
///
/// For the following chain:
///   B1-C1-D1-E1
///  /
/// A
///  \
///   B2-C2-D2-E2
///
/// Some scenarios and expected behavior:
/// nbb(C1), f(C1) -> false (handle_enactment was already performed in nbb(C1))
/// f(C1), nbb(C1) -> false (handle_enactment was already performed in f(C1))
///
/// f(C1), nbb(D2) -> false (handle_enactment was already performed in f(C1), we should not retract
/// finalized block)
/// f(C1), f(C2), nbb(C1) -> false
/// nbb(C1), nbb(C2) -> true (switching fork is OK)
/// nbb(B1), nbb(B2) -> true
/// nbb(B1), nbb(C1), f(C1) -> false (handle_enactment was already performed in nbb(B1)
/// nbb(C1), f(B1) -> false (handle_enactment was already performed in nbb(B2)
pub trait EnactmentPolicy<Block: BlockT> {
	/// Basing on provided ChainEvent returns true if maintainance/enactment procedure for evented
	/// header shall be performed. Otherwise returns false.
	/// Additionally tree_route from recent best_block to event's hash is returned.
	fn should_handle_enactment(
		&mut self,
		event: &ChainEvent<Block>,
	) -> (bool, Option<Arc<TreeRoute<Block>>>);
}

struct EnactmentHelper<Block, PoolApi>
where
	Block: BlockT,
	PoolApi: graph::ChainApi<Block = Block>,
{
	best_block: Option<Block::Hash>,
	finalized_block: Option<Block::Hash>,
	api: Arc<PoolApi>,
}

impl<Block, PoolApi> EnactmentPolicy<Block> for EnactmentHelper<Block, PoolApi>
where
	Block: BlockT,
	PoolApi: 'static + graph::ChainApi<Block = Block>,
{
	fn should_handle_enactment(
		&mut self,
		event: &ChainEvent<Block>,
	) -> (bool, Option<Arc<TreeRoute<Block>>>) {
		let (hash, finalized) = match event {
			ChainEvent::NewBestBlock { hash, .. } => (hash, false),
			ChainEvent::Finalized { hash, .. } => (hash, true),
		};

		//compute t
		let tree_route = if let Some(best_block) = self.best_block {
			Some(Arc::new(self.api.tree_route(best_block, *hash).unwrap()))
		} else {
			None
		};

		let result = self.resolve(*hash, tree_route.clone(), finalized);
		(result, tree_route)
	}
}

impl<Block, PoolApi> EnactmentHelper<Block, PoolApi>
where
	Block: BlockT,
	PoolApi: 'static + graph::ChainApi<Block = Block>,
{
	fn new(api: Arc<PoolApi>) -> Self {
		EnactmentHelper { best_block: None, finalized_block: None, api }
	}

	fn resolve(
		&mut self,
		hash: Block::Hash,
		tree_route: Option<Arc<TreeRoute<Block>>>,
		finalized: bool,
	) -> bool {
		let finalized_block = &mut self.finalized_block;
		let best_block = &mut self.best_block;
		log::trace!(target: "txpool", "resolve hash:{hash:?} finalized:{finalized:?} tree_route:{tree_route:?}, best_block:{best_block:?}, finalized_block:{finalized_block:?}");

		//block was already finalized
		if let Some(finalized_block) = *finalized_block {
			if finalized_block == hash {
				log::trace!(target:"txpool", "handle_enactment: block already finalized: exit 3b");
				return false
			}
		}

		// check if recently finalized block is on retracted path...
		let was_finalized_retracted = if let Some(finalized_block) = *finalized_block {
			match tree_route {
				Some(ref tr) => tr.retracted().iter().map(|x| x.hash).any(|x| x == finalized_block),
				None => false,
			}
		} else {
			false
		};

		if was_finalized_retracted {
			log::trace!(target: "txpool", "handle_enactment: recently finalized block is on retracted path: exit 1");
			return false
		}

		// If there are no enacted blocks in best_block -> hash tree_route, it means that
		// block being finalized was already enacted. (This case also covers best_block == hash)
		if finalized {
			if let Some(ref tree_route) = tree_route {
				if tree_route.enacted().len() == 0 {
					log::trace!(
						target: "txpool",
						"handle_enactment: no newly enacted blocks since recent best block: exit 2"
					);
					*finalized_block = Some(hash);
					return false
				}
			}
		}

		log::trace!(target: "txpool", "handle_enactment: proceed....");

		if finalized {
			*finalized_block = Some(hash);

			//check if the recent best_block was retracted
			let best_block_retracted = if let Some(best_block) = *best_block {
				match tree_route {
					Some(tr) => tr.retracted().iter().map(|x| x.hash).any(|x| x == best_block),
					None => false,
				}
			} else {
				false
			};

			//...if it was retracted, or was not set, newly finalized block becomes new best_block
			if best_block_retracted || best_block.is_none() {
				*best_block = Some(hash)
			}
		} else {
			*best_block = Some(hash);
		}

		return true
	}
}

/// Inform the transaction pool about imported and finalized blocks.
pub async fn notification_future<Client, Pool, Block>(client: Arc<Client>, txpool: Arc<Pool>)
where
	Block: BlockT,
	Client: sc_client_api::BlockchainEvents<Block>,
	Pool: MaintainedTransactionPool<Block = Block>,
{
	let import_stream = client
		.import_notification_stream()
		.filter_map(|n| ready(n.try_into().ok()))
		.fuse();
	let finality_stream = client.finality_notification_stream().map(Into::into).fuse();

	futures::stream::select(import_stream, finality_stream)
		.for_each(|evt| txpool.maintain(evt))
		.await
}
