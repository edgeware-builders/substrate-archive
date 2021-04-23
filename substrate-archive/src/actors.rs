// Copyright 2017-2021 Parity Technologies (UK) Ltd.
// This file is part of substrate-archive.

// substrate-archive is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// substrate-archive is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
// You should have received a copy of the GNU General Public License
// along with substrate-archive.  If not, see <http://www.gnu.org/licenses/>.

//! Main entrypoint for substrate-archive. `init` will start all actors and begin indexing the
//! chain defined with the passed-in Client and URL.

mod workers;

use std::{marker::PhantomData, panic::AssertUnwindSafe, sync::Arc, time::Duration};

use coil::Job as _;
use futures::{
	executor::block_on,
	future::{self, BoxFuture},
	FutureExt,
};
use hashbrown::HashSet;
use serde::{de::DeserializeOwned, Deserialize};
use xtra::{prelude::*, Disconnected};

use sc_client_api::backend;
use sp_api::{ApiExt, ConstructRuntimeApi};
use sp_block_builder::BlockBuilder as BlockBuilderApi;
use sp_runtime::traits::{Block as BlockT, Header as _, NumberFor};

use substrate_archive_backend::{ApiAccess, Meta, ReadOnlyBackend, ReadOnlyDb};

use self::workers::{
	blocks::{Crawl, ReIndex},
	storage_aggregator::{SendStorage, SendTraces},
	GetState,
};
pub use self::workers::{BlocksIndexer, DatabaseActor, StorageAggregator};
use crate::{
	archive::Archive,
	database::{models::BlockModelDecoder, queries, Channel, Listener},
	error::Result,
	tasks::Environment,
	types::Die,
};
use easy_parallel::Parallel;

// TODO: Split this up into two objects
// System should be a factory that produces objects that should be spawned

/// Provides parameters that are passed in from the user.
/// Provides context that every actor may use
pub struct SystemConfig<B, D> {
	pub backend: Arc<ReadOnlyBackend<B, D>>,
	pub pg_url: String,
	pub meta: Meta<B>,
	pub control: ControlConfig,
	pub tracing_targets: Option<String>,
	pub executor: Arc<smol::Executor<'static>>,
}

impl<B, D> Clone for SystemConfig<B, D> {
	fn clone(&self) -> SystemConfig<B, D> {
		SystemConfig {
			backend: Arc::clone(&self.backend),
			pg_url: self.pg_url.clone(),
			meta: self.meta.clone(),
			control: self.control,
			tracing_targets: self.tracing_targets.clone(),
			executor: self.executor.clone(),
		}
	}
}

#[derive(Copy, Clone, Debug, Deserialize)]
pub struct ControlConfig {
	/// Number of threads to spawn for task execution.
	#[serde(default = "default_task_workers")]
	pub(crate) task_workers: usize,
	/// Maximum amount of time coil will wait for a task to begin.
	/// Times out if tasks don't start execution in the thread pool within `task_timeout` seconds.
	#[serde(default = "default_task_timeout")]
	pub(crate) task_timeout: u64,
	/// Maximum tasks to queue in the threadpool.
	#[serde(default = "default_task_workers")]
	pub(crate) max_tasks: usize,
	/// Maximum amount of blocks to index at once.
	#[serde(default = "default_max_block_load")]
	pub(crate) max_block_load: u32,
}

impl Default for ControlConfig {
	fn default() -> Self {
		Self {
			task_workers: default_task_workers(),
			task_timeout: default_task_timeout(),
			max_tasks: default_max_tasks(),
			max_block_load: default_max_block_load(),
		}
	}
}

fn default_task_workers() -> usize {
	num_cpus::get()
}

const fn default_task_timeout() -> u64 {
	20
}

const fn default_max_tasks() -> usize {
	64
}

const fn default_max_block_load() -> u32 {
	100_000
}

impl<B: BlockT + Unpin, D: ReadOnlyDb> SystemConfig<B, D>
where
	B::Hash: Unpin,
{
	pub fn new(
		backend: Arc<ReadOnlyBackend<B, D>>,
		pg_url: String,
		meta: Meta<B>,
		control: ControlConfig,
		tracing_targets: Option<String>,
	) -> Self {
		let executor = Arc::new(smol::Executor::new());
		Self { backend, pg_url, meta, control, tracing_targets, executor }
	}

	pub fn backend(&self) -> &Arc<ReadOnlyBackend<B, D>> {
		&self.backend
	}

	pub fn pg_url(&self) -> &str {
		self.pg_url.as_str()
	}

	pub fn meta(&self) -> &Meta<B> {
		&self.meta
	}
}

struct Actors<Block: Send + Sync + 'static, H: Send + Sync + 'static, Db: Send + Sync + 'static> {
	storage: Address<workers::StorageAggregator<H>>,
	blocks: Address<workers::BlocksIndexer<Block, Db>>,
	metadata: Address<workers::MetadataActor<Block>>,
	db: Address<DatabaseActor>,
}

impl<B: Send + Sync + 'static, H: Send + Sync + 'static, D: Send + Sync + 'static> Clone for Actors<B, H, D> {
	fn clone(&self) -> Self {
		Self {
			storage: self.storage.clone(),
			blocks: self.blocks.clone(),
			metadata: self.metadata.clone(),
			db: self.db.clone(),
		}
	}
}

impl<Block, Db> Actors<Block, Block::Hash, Db>
where
	Block: BlockT + Unpin,
	Db: ReadOnlyDb + 'static,
	Block::Hash: Unpin,
	NumberFor<Block>: Into<u32>,
{
	async fn spawn(conf: &SystemConfig<Block, Db>) -> Result<Self> {
		let db = workers::DatabaseActor::new(conf.pg_url().into(), conf.executor.clone()).await?.create(None);
		let (db, fut) = db.run();
		conf.executor.spawn(fut).detach();
		let storage = workers::StorageAggregator::new(db.clone()).create(None);
		let metadata = workers::MetadataActor::new(db.clone(), conf.meta().clone()).await?.create(None);
		let (storage, fut) = storage.run();
		conf.executor.spawn(fut).detach();
		let (metadata, fut) = metadata.run();
		conf.executor.spawn(fut).detach();
		let blocks = workers::BlocksIndexer::new(&conf, db.clone(), metadata.clone()).create(None);
		let (blocks, fut) = blocks.run();
		conf.executor.spawn(fut).detach();

		Ok(Actors { storage, blocks, metadata, db })
	}

	// Run a future that sends actors a signal to progress every X seconds
	async fn tick_interval(&self, executor: Arc<smol::Executor<'_>>) -> Result<()> {
		// messages that only need to be sent once
		self.blocks.send(ReIndex).await?;
		let actors = self.clone();
		let _handle = executor
			.spawn(async move {
				loop {
					let fut = (
						Box::pin(actors.blocks.send(Crawl)),
						Box::pin(actors.storage.send(SendStorage)),
						Box::pin(actors.storage.send(SendTraces)),
					);
					if let (Err(_), Err(_), Err(_)) = future::join3(fut.0, fut.1, fut.2).await {
						log::info!("Tick stopping");
						break;
					}
				}
			})
			.detach();
		Ok(())
	}

	async fn kill(self) -> Result<()> {
		let fut: Vec<BoxFuture<'_, Result<(), Disconnected>>> = vec![
			Box::pin(self.storage.send(Die)),
			Box::pin(self.blocks.send(Die)),
			Box::pin(self.metadata.send(Die)),
			Box::pin(self.db.send(Die)),
		];
		log::info!("Killing actors");
		future::join_all(fut).await;
		log::info!("futures joined");
		Ok(())
	}
}

/// Control the execution of the indexing engine.
/// Will exit on Drop.
pub struct System<B, R, C, D>
where
	D: ReadOnlyDb + 'static,
	B: BlockT + Unpin,
	B::Hash: Unpin,
	NumberFor<B>: Into<u32>,
{
	config: SystemConfig<B, D>,
	start_tx: flume::Sender<()>,
	kill_tx: flume::Sender<()>,
	/// handle to the futures runtime indexing the running chain
	handle: jod_thread::JoinHandle<Result<()>>,
	_marker: PhantomData<(B, R, C, D)>,
}

impl<B, R, C, D> System<B, R, C, D>
where
	D: ReadOnlyDb + 'static,
	B: BlockT + Unpin + DeserializeOwned,
	R: ConstructRuntimeApi<B, C> + Send + Sync + 'static,
	R::RuntimeApi: BlockBuilderApi<B>
		+ sp_api::Metadata<B>
		+ ApiExt<B, StateBackend = backend::StateBackendFor<ReadOnlyBackend<B, D>, B>>
		+ Send
		+ Sync
		+ 'static,
	C: ApiAccess<B, ReadOnlyBackend<B, D>, R> + 'static,
	NumberFor<B>: Into<u32> + From<u32> + Unpin,
	B::Hash: Unpin,
	B::Header: serde::de::DeserializeOwned,
{
	/// Initialize substrate archive.
	/// Requires a substrate client, url to a running RPC node, and a list of keys to index from storage.
	/// Optionally accepts a URL to the postgreSQL database. However, this can be defined as the
	/// environment variable `DATABASE_URL` instead.
	pub fn new(
		// one client per-threadpool. This way we don't have conflicting cache resources
		// for WASM runtime-instances
		client_api: Arc<C>,
		config: SystemConfig<B, D>,
	) -> Result<Self> {
		let (start_tx, kill_tx, handle) = Self::start(config.clone(), client_api);

		Ok(Self { config, start_tx, kill_tx, handle, _marker: PhantomData })
	}

	fn drive(&self) {
		self.start_tx.send(()).expect("Could not start actors");
	}

	/// Start the actors and begin driving their execution
	pub fn start(
		conf: SystemConfig<B, D>,
		client: Arc<C>,
	) -> (flume::Sender<()>, flume::Sender<()>, jod_thread::JoinHandle<Result<()>>) {
		let (tx_start, rx_start) = flume::bounded(1);
		let (tx_kill, rx_kill) = flume::bounded(1);

		let executor = conf.executor.clone();
		let handle = jod_thread::spawn(move || {
			// block until we receive the message to start
			let _ = rx_start.recv();
			let rx_kill_2 = rx_kill.clone();
			let (left_res, right_res) = Parallel::new()
				.each(0..conf.control.task_workers, |_| block_on(executor.run(rx_kill_2.recv_async())))
				.finish(|| block_on(Self::main_loop(conf, rx_kill, client)));
			match left_res.into_iter().collect::<Result<Vec<()>, flume::RecvError>>() {
				Err(flume::RecvError::Disconnected) => log::warn!("Senders dropped connection"),
				_ => (),
			};
			right_res?;
			Ok(())
		});

		(tx_start, tx_kill, handle)
	}
	/*
		async fn setup_actors(conf: &SystemConfig<B, D>) -> Result<(Actors<B, B::Hash, D>, PgPool)> {
			let actors = Actors::spawn(conf).await?;
			actors.index(conf.executor.clone()).await;
			let pool = actors.db.send(GetState::Pool).await??.pool();
			let listener = Self::init_listeners(&conf).await?;
			let mut conn = pool.acquire().await?;
			Self::restore_missing_storage(&mut *conn).await?;
			Ok((actors, pool))
		}
	*/
	async fn main_loop(conf: SystemConfig<B, D>, rx: flume::Receiver<()>, client: Arc<C>) -> Result<()> {
		let actors = Actors::spawn(&conf).await?;
		actors.tick_interval(conf.executor.clone()).await?;
		let pool = actors.db.send(GetState::Pool).await??.pool();
		let listener = Self::init_listeners(&conf).await?;
		let mut conn = pool.acquire().await?;
		Self::restore_missing_storage(&mut *conn).await?;

		let env = Environment::<B, B::Hash, R, C, D>::new(
			conf.backend().clone(),
			client,
			actors.storage.clone(),
			conf.tracing_targets.clone(),
		);
		let env = AssertUnwindSafe(env);

		let runner = coil::Runner::builder(env, &pool)
			.register_job::<crate::tasks::execute_block::Job<B, R, C, D>>()
			.num_threads(conf.control.task_workers)
			// times out if tasks don't start execution on the threadpool within 20 seconds.
			.timeout(Duration::from_secs(conf.control.task_timeout))
			.build()?;

		loop {
			match rx.try_recv() {
				Err(flume::TryRecvError::Empty) => log::info!("Recv chan empty"),
				Err(flume::TryRecvError::Disconnected) => break,
				Ok(_) => {
					log::info!("Active Threads: {}, queued jobs: {}", runner.active_count(), runner.queued_count());
					log::info!("closing main loop");
					break;
				}
			}

			match runner.run_pending_tasks() {
				Ok(_) => (),
				Err(coil::FetchError::Timeout) => log::warn!("Tasks timed out"),
				Err(e) => log::error!("{:?}", e),
			}
		}
		listener.kill().await;
		actors.kill().await?;
		Ok(())
	}

	async fn init_listeners(conf: &SystemConfig<B, D>) -> Result<Listener> {
		Listener::builder(conf.pg_url(), move |notif, conn| {
			async move {
				let block = queries::get_full_block_by_number(conn, notif.block_num).await?;
				let b: (B, u32) = BlockModelDecoder::with_single(block)?;
				crate::tasks::execute_block::<B, R, C, D>(b.0, PhantomData).enqueue(conn).await?;
				Ok(())
			}
			.boxed()
		})
		.listen_on(Channel::Blocks)
		.spawn(&conf.executor)
		.await
	}

	/// Checks if any blocks that should be executed are missing
	/// from the task queue.
	/// If any are found, they are re-queued.
	async fn restore_missing_storage(conn: &mut sqlx::PgConnection) -> Result<()> {
		let blocks: HashSet<u32> = queries::get_all_blocks::<B>(conn)
			.await?
			.map(|b| Ok((*b?.header().number()).into()))
			.collect::<Result<_>>()?;
		let mut missing_storage_blocks = queries::blocks_storage_intersection(conn).await?;
		let difference: HashSet<u32> = missing_storage_blocks
			.iter()
			.map(|b| b.block_num as u32)
			.collect::<HashSet<u32>>()
			.difference(&blocks)
			.copied()
			.collect();
		missing_storage_blocks.retain(|b| difference.contains(&(b.block_num as u32)));
		let jobs: Vec<crate::tasks::execute_block::Job<B, R, C, D>> =
			BlockModelDecoder::with_vec(missing_storage_blocks)?
				.into_iter()
				.map(|b| crate::tasks::execute_block::<B, R, C, D>(b.inner.block, PhantomData))
				.collect();
		log::info!("Restoring {} missing storage entries. This could take a few minutes...", jobs.len());
		coil::JobExt::enqueue_batch(jobs, &mut *conn).await?;
		log::info!("Storage restored");
		Ok(())
	}
}

#[async_trait::async_trait(?Send)]
impl<B, R, C, D> Archive<B, D> for System<B, R, C, D>
where
	D: ReadOnlyDb + 'static,
	B: BlockT + Unpin + DeserializeOwned,
	<B as BlockT>::Hash: Unpin,
	R: ConstructRuntimeApi<B, C> + Send + Sync + 'static,
	R::RuntimeApi: BlockBuilderApi<B>
		+ sp_api::Metadata<B>
		+ ApiExt<B, StateBackend = backend::StateBackendFor<ReadOnlyBackend<B, D>, B>>
		+ Send
		+ Sync
		+ 'static,
	C: ApiAccess<B, ReadOnlyBackend<B, D>, R> + 'static,
	NumberFor<B>: Into<u32> + From<u32> + Unpin,
	B::Hash: Unpin,
	B::Header: serde::de::DeserializeOwned,
{
	fn drive(&mut self) -> Result<()> {
		System::drive(self);
		Ok(())
	}

	async fn block_until_stopped(&self) {
		loop {
			smol::Timer::after(std::time::Duration::from_secs(1)).await;
		}
	}

	fn shutdown(self) -> Result<()> {
		let mut count = 0;
		loop {
			match self.kill_tx.send(()) {
				Err(_) => break, // everyone got the shutdown signal
				Ok(_) => {
					if count % 2 == 0 {
						log::info!("Sending!");
					}
					// some receivers may still be alive
					std::thread::sleep(std::time::Duration::from_millis(20));
					count += 1;
					continue;
				}
			}
		}
		self.handle.join()?;
		Ok(())
	}

	fn boxed_shutdown(self: Box<Self>) -> Result<()> {
		self.shutdown()
	}

	fn context(&self) -> &SystemConfig<B, D> {
		&self.config
	}
}
