// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Input quiesce of the virtio-net queue workers.
//!
//! Quiescing input stops the coordinator and the workers, lets each endpoint
//! queue finish the work it already accepted while it stops admitting new host
//! RX, drains the resulting completions, and restarts the workers. The
//! coordinator latches the input gate and re-establishes it whenever it
//! restarts the endpoint queues; resuming input releases it. Only devices built
//! with [`NicBuilder::save_restore`](crate::NicBuilder::save_restore) support
//! input quiesce.

use crate::Coordinator;
use crate::Device;
use crate::EndpointQueueState;
use crate::Worker;
use crate::WorkerError;
use anyhow::Context as _;
use std::future::pending;
use task_control::StopTask;

impl Device {
    /// Implements `quiesce_input`.
    pub(crate) async fn quiesce_endpoint_input(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.adapter.save_restore.is_some(),
            "virtio-net input quiesce is unavailable without saved-state configuration"
        );
        self.lifecycle.input_quiesced = true;
        self.quiesce_active_workers(true).await
    }

    /// Implements `resume_input`.
    pub(crate) async fn resume_endpoint_input(&mut self) -> anyhow::Result<()> {
        self.resume_quiesced_input().await?;
        self.lifecycle.input_quiesced = false;
        Ok(())
    }

    pub(crate) async fn quiesce_active_workers(&mut self, restart: bool) -> anyhow::Result<()> {
        if !self.coordinator.is_running() && self.coordinator.state().is_none() {
            return Ok(());
        }
        self.coordinator.stop().await;
        {
            let coordinator = self
                .coordinator
                .state_mut()
                .context("virtio-net coordinator state is unavailable")?;
            coordinator.input_quiesced = self.lifecycle.input_quiesced;
            for worker in &mut coordinator.workers {
                worker.stop().await;
            }
            for worker in &mut coordinator.workers {
                let (queue, state) = worker.get_mut();
                let state =
                    state.context("virtio-net worker state is unavailable during quiesce")?;
                if let Some(queue) = queue.state.as_mut() {
                    state
                        .quiesce_endpoint(queue)
                        .await
                        .map_err(anyhow::Error::new)?;
                }
            }
            if restart {
                for worker in &mut coordinator.workers {
                    worker.start();
                }
            }
        }
        if restart {
            self.coordinator.start();
        }
        Ok(())
    }

    async fn resume_quiesced_input(&mut self) -> anyhow::Result<()> {
        if !self.coordinator.is_running() && self.coordinator.state().is_none() {
            return Ok(());
        }
        self.coordinator.stop().await;
        {
            let coordinator = self
                .coordinator
                .state_mut()
                .context("virtio-net coordinator state is unavailable")?;
            coordinator.input_quiesced = false;
            for worker in &mut coordinator.workers {
                worker.stop().await;
            }
            for worker in &mut coordinator.workers {
                let (queue, state) = worker.get_mut();
                let state = state.context("virtio-net worker state is unavailable")?;
                if let Some(queue) = queue.state.as_mut() {
                    state.resume_endpoint(queue)?;
                }
                worker.start();
            }
        }
        self.coordinator.start();
        Ok(())
    }
}

impl Coordinator {
    /// Re-establishes a latched input gate after the endpoint queues restart.
    ///
    /// If the gate cannot be established, waits until the coordinator is
    /// stopped instead of starting the workers.
    pub(crate) async fn establish_input_gate(
        &mut self,
        stop: &mut StopTask<'_>,
    ) -> Result<(), task_control::Cancelled> {
        if self.input_quiesced
            && let Err(err) = stop.until_stopped(self.quiesce_workers()).await?
        {
            tracing::error!(
                error = %err,
                "failed to establish virtio-net input gate"
            );
            stop.until_stopped(pending::<()>()).await?;
        }
        Ok(())
    }

    async fn quiesce_workers(&mut self) -> anyhow::Result<()> {
        for worker in &mut self.workers {
            let (queue, state) = worker.get_mut();
            let state = state.context("virtio-net worker state is unavailable")?;
            if let Some(queue) = queue.state.as_mut() {
                state
                    .quiesce_endpoint(queue)
                    .await
                    .map_err(anyhow::Error::new)?;
            }
        }
        Ok(())
    }
}

impl Worker {
    async fn quiesce_endpoint(
        &mut self,
        queue_state: &mut EndpointQueueState,
    ) -> Result<(), WorkerError> {
        for _ in 0..=usize::from(self.virtio_state.tx_queue_size) {
            while self.transmit_pending_segments(queue_state)? {}

            queue_state
                .queue
                .quiesce(&mut self.active_state.pending_rx_packets)
                .await
                .map_err(WorkerError::Endpoint)?;
            self.process_endpoint_rx(queue_state.queue.as_mut())?;
            self.process_endpoint_tx(queue_state.queue.as_mut())?;

            if self.active_state.data.tx_segments.is_empty()
                && self
                    .active_state
                    .pending_tx_packets
                    .iter()
                    .all(Option::is_none)
            {
                let ownership = queue_state
                    .queue
                    .quiesce(&mut self.active_state.pending_rx_packets)
                    .await
                    .map_err(WorkerError::Endpoint)?;
                if ownership.rx_ready != 0 || ownership.tx_ready != 0 {
                    return Err(WorkerError::Endpoint(anyhow::anyhow!(
                        "network endpoint retained completions after quiesce drain"
                    )));
                }
                return Ok(());
            }
        }
        Err(WorkerError::Endpoint(anyhow::anyhow!(
            "network endpoint did not drain within the queue-size bound"
        )))
    }

    fn resume_endpoint(&mut self, queue_state: &mut EndpointQueueState) -> anyhow::Result<()> {
        queue_state.queue.resume()?;
        let count = self
            .active_state
            .pending_rx_packets
            .fill_ready(&mut self.active_state.data.rx_ready);
        queue_state.queue.rx_avail(
            &mut self.active_state.pending_rx_packets,
            &self.active_state.data.rx_ready[..count],
        );
        Ok(())
    }
}
