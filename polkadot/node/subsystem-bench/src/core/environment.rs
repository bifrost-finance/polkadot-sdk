// Copyright (C) Parity Technologies (UK) Ltd.
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
//! Test environment implementation
use crate::{
	core::{mock::AlwaysSupportsParachains, network::NetworkEmulatorHandle},
	TestConfiguration,
};
use colored::Colorize;
use core::time::Duration;
use futures::{Future, FutureExt};
use polkadot_overseer::{BlockInfo, Handle as OverseerHandle};

use polkadot_node_subsystem::{messages::AllMessages, Overseer, SpawnGlue, TimeoutExt};
use polkadot_node_subsystem_types::Hash;
use polkadot_node_subsystem_util::metrics::prometheus::{
	self, Gauge, Histogram, PrometheusError, Registry, U64,
};

use sc_service::{SpawnTaskHandle, TaskManager};
use std::net::{Ipv4Addr, SocketAddr};
use tokio::runtime::Handle;

const LOG_TARGET: &str = "subsystem-bench::environment";
use super::configuration::TestAuthorities;

/// Test environment/configuration metrics
#[derive(Clone)]
pub struct TestEnvironmentMetrics {
	/// Number of bytes sent per peer.
	n_validators: Gauge<U64>,
	/// Number of received sent per peer.
	n_cores: Gauge<U64>,
	/// PoV size
	pov_size: Histogram,
	/// Current block
	current_block: Gauge<U64>,
	/// Current block
	block_time: Gauge<U64>,
}

impl TestEnvironmentMetrics {
	pub fn new(registry: &Registry) -> Result<Self, PrometheusError> {
		let buckets = prometheus::exponential_buckets(16384.0, 2.0, 9)
			.expect("arguments are always valid; qed");

		Ok(Self {
			n_validators: prometheus::register(
				Gauge::new(
					"subsystem_benchmark_n_validators",
					"Total number of validators in the test",
				)?,
				registry,
			)?,
			n_cores: prometheus::register(
				Gauge::new(
					"subsystem_benchmark_n_cores",
					"Number of cores we fetch availability for each block",
				)?,
				registry,
			)?,
			current_block: prometheus::register(
				Gauge::new("subsystem_benchmark_current_block", "The current test block")?,
				registry,
			)?,
			block_time: prometheus::register(
				Gauge::new("subsystem_benchmark_block_time", "The time it takes for the target subsystems(s) to complete all the requests in a block")?,
				registry,
			)?,
			pov_size: prometheus::register(
				Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"subsystem_benchmark_pov_size",
						"The compressed size of the proof of validity of a candidate",
					)
					.buckets(buckets),
				)?,
				registry,
			)?,
		})
	}

	pub fn set_n_validators(&self, n_validators: usize) {
		self.n_validators.set(n_validators as u64);
	}

	pub fn set_n_cores(&self, n_cores: usize) {
		self.n_cores.set(n_cores as u64);
	}

	pub fn set_current_block(&self, current_block: usize) {
		self.current_block.set(current_block as u64);
	}

	pub fn set_block_time(&self, block_time_ms: u64) {
		self.block_time.set(block_time_ms);
	}

	pub fn on_pov_size(&self, pov_size: usize) {
		self.pov_size.observe(pov_size as f64);
	}
}

fn new_runtime() -> tokio::runtime::Runtime {
	tokio::runtime::Builder::new_multi_thread()
		.thread_name("subsystem-bench")
		.enable_all()
		.thread_stack_size(3 * 1024 * 1024)
		.build()
		.unwrap()
}

/// Wrapper for dependencies
pub struct TestEnvironmentDependencies {
	pub registry: Registry,
	pub task_manager: TaskManager,
	pub runtime: tokio::runtime::Runtime,
}

impl Default for TestEnvironmentDependencies {
	fn default() -> Self {
		let runtime = new_runtime();
		let registry = Registry::new();
		let task_manager: TaskManager =
			TaskManager::new(runtime.handle().clone(), Some(&registry)).unwrap();

		Self { runtime, registry, task_manager }
	}
}

// A dummy genesis hash
pub const GENESIS_HASH: Hash = Hash::repeat_byte(0xff);

// We use this to bail out sending messages to the subsystem if it is overloaded such that
// the time of flight is breaches 5s.
// This should eventually be a test parameter.
pub const MAX_TIME_OF_FLIGHT: Duration = Duration::from_millis(5000);

/// The test environment is the high level wrapper of all things required to test
/// a certain subsystem.
///
/// ## Mockups
/// The overseer is passed in during construction and it can host an arbitrary number of
/// real subsystems instances and the corresponding mocked instances such that the real
/// subsystems can get their messages answered.
///
/// As the subsystem's performance depends on network connectivity, the test environment
/// emulates validator nodes on the network, see `NetworkEmulator`. The network emulation
/// is configurable in terms of peer bandwidth, latency and connection error rate using
/// uniform distribution sampling.
///
///
/// ## Usage
/// `TestEnvironment` is used in tests to send `Overseer` messages or signals to the subsystem
/// under test.
///
/// ## Collecting test metrics
///
/// ### Prometheus
/// A prometheus endpoint is exposed while the test is running. A local Prometheus instance
/// can scrape it every 1s and a Grafana dashboard is the preferred way of visualizing
/// the performance characteristics of the subsystem.
///
/// ### CLI
/// A subset of the Prometheus metrics are printed at the end of the test.
pub struct TestEnvironment {
	/// Test dependencies
	dependencies: TestEnvironmentDependencies,
	/// A runtime handle
	runtime_handle: tokio::runtime::Handle,
	/// A handle to the lovely overseer
	overseer_handle: OverseerHandle,
	/// The test configuration.
	config: TestConfiguration,
	/// A handle to the network emulator.
	network: NetworkEmulatorHandle,
	/// Configuration/env metrics
	metrics: TestEnvironmentMetrics,
	/// Test authorities generated from the configuration.
	authorities: TestAuthorities,
}

impl TestEnvironment {
	/// Create a new test environment
	pub fn new(
		dependencies: TestEnvironmentDependencies,
		config: TestConfiguration,
		network: NetworkEmulatorHandle,
		overseer: Overseer<SpawnGlue<SpawnTaskHandle>, AlwaysSupportsParachains>,
		overseer_handle: OverseerHandle,
		authorities: TestAuthorities,
	) -> Self {
		let metrics = TestEnvironmentMetrics::new(&dependencies.registry)
			.expect("Metrics need to be registered");

		let spawn_handle = dependencies.task_manager.spawn_handle();
		spawn_handle.spawn_blocking("overseer", "overseer", overseer.run().boxed());

		let registry_clone = dependencies.registry.clone();
		dependencies.task_manager.spawn_handle().spawn_blocking(
			"prometheus",
			"test-environment",
			async move {
				prometheus_endpoint::init_prometheus(
					SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 9999),
					registry_clone,
				)
				.await
				.unwrap();
			},
		);

		TestEnvironment {
			runtime_handle: dependencies.runtime.handle().clone(),
			dependencies,
			overseer_handle,
			config,
			network,
			metrics,
			authorities,
		}
	}

	/// Returns the test configuration.
	pub fn config(&self) -> &TestConfiguration {
		&self.config
	}

	/// Returns a reference to the inner network emulator handle.
	pub fn network(&self) -> &NetworkEmulatorHandle {
		&self.network
	}

	/// Returns the Prometheus registry.
	pub fn registry(&self) -> &Registry {
		&self.dependencies.registry
	}

	/// Spawn a named task in the `test-environment` task group.
	#[allow(unused)]
	pub fn spawn(&self, name: &'static str, task: impl Future<Output = ()> + Send + 'static) {
		self.dependencies
			.task_manager
			.spawn_handle()
			.spawn(name, "test-environment", task);
	}

	/// Spawn a blocking named task in the `test-environment` task group.
	pub fn spawn_blocking(
		&self,
		name: &'static str,
		task: impl Future<Output = ()> + Send + 'static,
	) {
		self.dependencies.task_manager.spawn_handle().spawn_blocking(
			name,
			"test-environment",
			task,
		);
	}
	/// Returns a reference to the test environment metrics instance
	pub fn metrics(&self) -> &TestEnvironmentMetrics {
		&self.metrics
	}

	/// Returns a handle to the tokio runtime.
	pub fn runtime(&self) -> Handle {
		self.runtime_handle.clone()
	}

	/// Returns a reference to the authority keys used in the test.
	pub fn authorities(&self) -> &TestAuthorities {
		&self.authorities
	}

	/// Send a message to the subsystem under test environment.
	pub async fn send_message(&mut self, msg: AllMessages) {
		self.overseer_handle
			.send_msg(msg, LOG_TARGET)
			.timeout(MAX_TIME_OF_FLIGHT)
			.await
			.unwrap_or_else(|| {
				panic!("{}ms maximum time of flight breached", MAX_TIME_OF_FLIGHT.as_millis())
			});
	}

	/// Send an `ActiveLeavesUpdate` signal to all subsystems under test.
	pub async fn import_block(&mut self, block: BlockInfo) {
		self.overseer_handle
			.block_imported(block)
			.timeout(MAX_TIME_OF_FLIGHT)
			.await
			.unwrap_or_else(|| {
				panic!("{}ms maximum time of flight breached", MAX_TIME_OF_FLIGHT.as_millis())
			});
	}

	/// Stop overseer and subsystems.
	pub async fn stop(&mut self) {
		self.overseer_handle.stop().await;
	}

	/// Blocks until `metric_name` == `value`
	pub async fn wait_until_metric_eq(&self, metric_name: &str, value: usize) {
		let value = value as f64;
		loop {
			let test_metrics = super::display::parse_metrics(self.registry());
			let current_value = test_metrics.sum_by(metric_name);

			gum::debug!(target: LOG_TARGET, metric_name, current_value, value, "Waiting for metric");
			if current_value == value {
				break
			}

			// Check value every 50ms.
			tokio::time::sleep(std::time::Duration::from_millis(50)).await;
		}
	}

	/// Display network usage stats.
	pub fn display_network_usage(&self) {
		let stats = self.network().peer_stats(0);

		let total_node_received = stats.received() / 1024;
		let total_node_sent = stats.sent() / 1024;

		println!(
			"\nPayload bytes received from peers: {}, {}",
			format!("{:.2} KiB total", total_node_received).blue(),
			format!("{:.2} KiB/block", total_node_received / self.config().num_blocks)
				.bright_blue()
		);

		println!(
			"Payload bytes sent to peers: {}, {}",
			format!("{:.2} KiB total", total_node_sent).blue(),
			format!("{:.2} KiB/block", total_node_sent / self.config().num_blocks).bright_blue()
		);
	}

	/// Print CPU usage stats in the CLI.
	pub fn display_cpu_usage(&self, subsystems_under_test: &[&str]) {
		let test_metrics = super::display::parse_metrics(self.registry());

		for subsystem in subsystems_under_test.iter() {
			let subsystem_cpu_metrics =
				test_metrics.subset_with_label_value("task_group", subsystem);
			let total_cpu = subsystem_cpu_metrics.sum_by("substrate_tasks_polling_duration_sum");
			println!(
				"{} CPU usage {}",
				subsystem.to_string().bright_green(),
				format!("{:.3}s", total_cpu).bright_purple()
			);
			println!(
				"{} CPU usage per block {}",
				subsystem.to_string().bright_green(),
				format!("{:.3}s", total_cpu / self.config().num_blocks as f64).bright_purple()
			);
		}

		let test_env_cpu_metrics =
			test_metrics.subset_with_label_value("task_group", "test-environment");
		let total_cpu = test_env_cpu_metrics.sum_by("substrate_tasks_polling_duration_sum");
		println!(
			"Total test environment CPU usage {}",
			format!("{:.3}s", total_cpu).bright_purple()
		);
		println!(
			"Test environment CPU usage per block {}",
			format!("{:.3}s", total_cpu / self.config().num_blocks as f64).bright_purple()
		)
	}
}
