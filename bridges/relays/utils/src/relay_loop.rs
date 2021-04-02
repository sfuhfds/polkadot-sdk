// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Parity Bridges Common.

// Parity Bridges Common is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Bridges Common is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Bridges Common.  If not, see <http://www.gnu.org/licenses/>.

use crate::metrics::{Metrics, MetricsParams, StandaloneMetrics};
use crate::{FailedClient, MaybeConnectionError};

use async_trait::async_trait;
use std::{fmt::Debug, future::Future, net::SocketAddr, time::Duration};
use substrate_prometheus_endpoint::{init_prometheus, Registry};

/// Default pause between reconnect attempts.
pub const RECONNECT_DELAY: Duration = Duration::from_secs(10);

/// Basic blockchain client from relay perspective.
#[async_trait]
pub trait Client: Clone + Send + Sync {
	/// Type of error this clients returns.
	type Error: Debug + MaybeConnectionError;

	/// Try to reconnect to source node.
	async fn reconnect(&mut self) -> Result<(), Self::Error>;
}

/// Returns generic loop that may be customized and started.
pub fn relay_loop<SC, TC>(source_client: SC, target_client: TC) -> Loop<SC, TC, ()> {
	Loop {
		reconnect_delay: RECONNECT_DELAY,
		source_client,
		target_client,
		loop_metric: None,
	}
}

/// Generic relay loop.
pub struct Loop<SC, TC, LM> {
	reconnect_delay: Duration,
	source_client: SC,
	target_client: TC,
	loop_metric: Option<LM>,
}

/// Relay loop metrics builder.
pub struct LoopMetrics<SC, TC, LM> {
	relay_loop: Loop<SC, TC, ()>,
	registry: Registry,
	loop_metric: Option<LM>,
}

impl<SC, TC, LM> Loop<SC, TC, LM> {
	/// Customize delay between reconnect attempts.
	pub fn reconnect_delay(mut self, reconnect_delay: Duration) -> Self {
		self.reconnect_delay = reconnect_delay;
		self
	}

	/// Start building loop metrics using given prefix.
	///
	/// Panics if `prefix` is empty.
	pub fn with_metrics(self, prefix: String) -> LoopMetrics<SC, TC, ()> {
		assert!(!prefix.is_empty(), "Metrics prefix can not be empty");

		LoopMetrics {
			relay_loop: Loop {
				reconnect_delay: self.reconnect_delay,
				source_client: self.source_client,
				target_client: self.target_client,
				loop_metric: None,
			},
			registry: Registry::new_custom(Some(prefix), None)
				.expect("only fails if prefix is empty; prefix is not empty; qed"),
			loop_metric: None,
		}
	}

	/// Run relay loop.
	///
	/// This function represents an outer loop, which in turn calls provided `run_loop` function to do
	/// actual job. When `run_loop` returns, this outer loop reconnects to failed client (source,
	/// target or both) and calls `run_loop` again.
	pub async fn run<R, F>(mut self, run_loop: R) -> Result<(), String>
	where
		R: Fn(SC, TC, Option<LM>) -> F,
		F: Future<Output = Result<(), FailedClient>>,
		SC: Client,
		TC: Client,
		LM: Clone,
	{
		loop {
			let result = run_loop(
				self.source_client.clone(),
				self.target_client.clone(),
				self.loop_metric.clone(),
			)
			.await;

			match result {
				Ok(()) => break,
				Err(failed_client) => loop {
					async_std::task::sleep(self.reconnect_delay).await;
					if failed_client == FailedClient::Both || failed_client == FailedClient::Source {
						match self.source_client.reconnect().await {
							Ok(()) => (),
							Err(error) => {
								log::warn!(
									target: "bridge",
									"Failed to reconnect to source client. Going to retry in {}s: {:?}",
									self.reconnect_delay.as_secs(),
									error,
								);
								continue;
							}
						}
					}
					if failed_client == FailedClient::Both || failed_client == FailedClient::Target {
						match self.target_client.reconnect().await {
							Ok(()) => (),
							Err(error) => {
								log::warn!(
									target: "bridge",
									"Failed to reconnect to target client. Going to retry in {}s: {:?}",
									self.reconnect_delay.as_secs(),
									error,
								);
								continue;
							}
						}
					}

					break;
				},
			}

			log::debug!(target: "bridge", "Restarting relay loop");
		}

		Ok(())
	}
}

impl<SC, TC, LM> LoopMetrics<SC, TC, LM> {
	/// Add relay loop metrics.
	///
	/// Loop metrics will be passed to the loop callback.
	pub fn loop_metric<NewLM: Metrics>(self, loop_metric: NewLM) -> Result<LoopMetrics<SC, TC, NewLM>, String> {
		loop_metric.register(&self.registry)?;

		Ok(LoopMetrics {
			relay_loop: self.relay_loop,
			registry: self.registry,
			loop_metric: Some(loop_metric),
		})
	}

	/// Add standalone metrics.
	pub fn standalone_metric<M: StandaloneMetrics>(self, standalone_metrics: M) -> Result<Self, String> {
		standalone_metrics.register(&self.registry)?;
		standalone_metrics.spawn();
		Ok(self)
	}

	/// Expose metrics using given params.
	///
	/// If `params` is `None`, metrics are not exposed.
	pub async fn expose(self, params: Option<MetricsParams>) -> Result<Loop<SC, TC, LM>, String> {
		if let Some(params) = params {
			let socket_addr = SocketAddr::new(
				params.host.parse().map_err(|err| {
					format!(
						"Invalid host {} is used to expose Prometheus metrics: {}",
						params.host, err,
					)
				})?,
				params.port,
			);

			let registry = self.registry;
			async_std::task::spawn(async move {
				let result = init_prometheus(socket_addr, registry).await;
				log::trace!(
					target: "bridge-metrics",
					"Prometheus endpoint has exited with result: {:?}",
					result,
				);
			});
		}

		Ok(Loop {
			reconnect_delay: self.relay_loop.reconnect_delay,
			source_client: self.relay_loop.source_client,
			target_client: self.relay_loop.target_client,
			loop_metric: self.loop_metric,
		})
	}
}