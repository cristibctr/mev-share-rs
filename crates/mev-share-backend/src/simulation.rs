//! MEV-Share simulation backend.

use futures_util::{stream::FuturesUnordered, Stream, StreamExt};
use mev_share_rpc_api::{SendBundleRequest, SimBundleOverrides, SimBundleResponse};
use pin_project_lite::pin_project;
use std::{
    collections::VecDeque,
    error::Error,
    future::Future,
    pin::Pin,
    sync::{atomic::AtomicU64, Arc},
    task::{ready, Context, Poll},
};
use tokio::sync::{mpsc, oneshot};

/// A type that can start a bundle simulation.
pub trait BundleSimulator: Unpin + Send + Sync {
    /// An in progress bundle simulation.
    type Simulation: Future<Output = BundleSimulationOutcome> + Unpin + Send + Sync;

    /// Starts a bundle simulation.
    fn simulate_bundle(
        &self,
        bundle: SendBundleRequest,
        sim_overrides: SimBundleOverrides,
    ) -> Self::Simulation;
}

/// Errors that can occur when simulating a bundle.
#[derive(Debug)]
pub enum BundleSimulationOutcome {
    /// The simulation was successful.
    Success(SimBundleResponse),
    /// The simulation failed and is not recoverable.
    Fatal(Box<dyn Error + Send + Sync>),
    /// The simulation failed and should be rescheduled.
    Reschedule(Box<dyn Error + Send + Sync>),
}

/// Frontend type that can communicate with [BundleSimulatorService].
#[derive(Debug, Clone)]
pub struct BundleSimulatorHandle {
    to_service: mpsc::UnboundedSender<BundleSimulatorMessage>,
    inner: Arc<BundleSimulatorInner>,
}

#[derive(Debug)]
struct BundleSimulatorInner {
    /// tracks the number of queued jobs.
    queued_jobs: AtomicU64,
}

// === impl BundleSimulatorHandle ===

impl BundleSimulatorHandle {
    /// Returns the number of queued jobs.
    pub fn queued_jobs(&self) -> u64 {
        self.inner.queued_jobs.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Adds a new bundle simulation to the queue.
    pub fn add_bundle_simulation(
        &self,
        request: SendBundleRequest,
        overrides: SimBundleOverrides,
        priority: SimulationPriority,
    ) {
        let (tx, _rx) = oneshot::channel();
        // TODO make async
        let _ = self.to_service.send(BundleSimulatorMessage::AddSimulation {
            request: SimulationRequest { request, priority, overrides },
            tx,
        });
    }

    /// Returns a new listener for simulation events.
    pub fn simulation_event_listener(&self) -> SimulationEventStream {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = self.to_service.send(BundleSimulatorMessage::AddEventListener(tx));
        SimulationEventStream { inner: rx }
    }
}

/// Provides a service for simulating bundles.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct BundleSimulatorService<Sim: BundleSimulator> {
    /// The current block number.
    current_block_number: u64,
    /// Creates new simulations.
    simulator: Sim,
    /// The current simulations.
    simulations: FuturesUnordered<Simulation<Sim::Simulation>>,
    high_priority_queue: VecDeque<SimulationRequest>,
    normal_priority_queue: VecDeque<SimulationRequest>,
    /// incoming messages from the handle.
    from_handle: mpsc::UnboundedReceiver<BundleSimulatorMessage>,
    /// Copy of the handle sender to keep the channel open.
    to_service: mpsc::UnboundedSender<BundleSimulatorMessage>,
    /// Shared internals
    inner: Arc<BundleSimulatorInner>,
    /// Listeners for simulation events.
    listeners: Vec<mpsc::UnboundedSender<SimulationEvent>>,
    /// How this service is configured.
    config: BundleSimulatorServiceConfig,
}

impl<Sim: BundleSimulator> BundleSimulatorService<Sim> {
    /// Creates a new [BundleSimulatorService] with the given simulator.
    pub fn new(
        current_block_number: u64,
        simulator: Sim,
        config: BundleSimulatorServiceConfig,
    ) -> Self {
        let (to_service, from_handle) = mpsc::unbounded_channel();
        let inner = Arc::new(BundleSimulatorInner { queued_jobs: AtomicU64::new(0) });
        Self {
            current_block_number,
            simulator,
            simulations: Default::default(),
            high_priority_queue: Default::default(),
            normal_priority_queue: Default::default(),
            from_handle,
            to_service,
            inner,
            listeners: vec![],
            config,
        }
    }

    /// Returns a new handle to this service
    pub fn handle(&self) -> BundleSimulatorHandle {
        BundleSimulatorHandle {
            to_service: self.to_service.clone(),
            inner: Arc::clone(&self.inner),
        }
    }

    /// Notifies all listeners of the given event.
    fn notify_listeners(&mut self, event: SimulationEvent) {
        self.listeners.retain(|l| l.send(event.clone()).is_ok());
    }

    /// Processes a new message from the handle.
    fn on_message(&mut self, msg: BundleSimulatorMessage) {
        match msg {
            BundleSimulatorMessage::UpdateBlockNumber(num) => {
                if num != self.current_block_number {
                    let old = self.current_block_number;
                    self.current_block_number = num;
                    self.notify_listeners(SimulationEvent::BlockNumberUpdated {
                        old,
                        current: self.current_block_number,
                    });
                }
            }
            BundleSimulatorMessage::ClearJobs { res: _ } => {
                // TODO clear
            }
            BundleSimulatorMessage::AddEventListener(tx) => {
                self.listeners.push(tx);
            }
            BundleSimulatorMessage::AddSimulation { request, tx } => {
                // TODO add simulation
            }
        }
    }

    /// Processes a finished simulation.
    fn on_simulation_outcome(&mut self, outcome: BundleSimulationOutcome, sim: SimulationInner) {
        match outcome {
            BundleSimulationOutcome::Success(resp) => {
                let SimulationInner { retries, request, overrides } = sim;
                self.notify_listeners(SimulationEvent::SimulatedBundle(Ok(SimulatedBundle {
                    request,
                    overrides,
                    response: resp,
                    retries,
                })));
            }
            BundleSimulationOutcome::Fatal(error) => {
                let err = SimulatedBundleErrorInner { error, sim };
                self.notify_listeners(SimulationEvent::SimulatedBundle(Err(
                    SimulatedBundleError { inner: Arc::new(err) },
                )));
            }
            BundleSimulationOutcome::Reschedule(_error) => {
                // reschedule the simulation.
                // TODO reschedule
            }
        }
    }
}

impl<Sim> Future for BundleSimulatorService<Sim>
where
    Sim: BundleSimulator,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // drain incoming messages from the handle.
        while let Poll::Ready(Some(msg)) = this.from_handle.poll_recv(cx) {
            this.on_message(msg);
        }

        // process all completed simulations.
        while let Poll::Ready(Some((outcome, inner))) = this.simulations.poll_next_unpin(cx) {
            this.on_simulation_outcome(outcome, inner);
        }

        Poll::Pending
    }
}

/// Config values for the [BundleSimulatorService].
#[derive(Debug, Clone)]
pub struct BundleSimulatorServiceConfig {
    /// Maximum number of retries for a simulation.
    pub max_retries: usize,
}

impl Default for BundleSimulatorServiceConfig {
    fn default() -> Self {
        Self { max_retries: 30 }
    }
}

pin_project! {
    /// A simulation future
    struct Simulation<Sim> {
        #[pin]
        sim: Sim,
        inner: Option<SimulationInner>
    }
}

impl<Sim> Simulation<Sim> {
    /// Creates a new simulation.
    fn new(sim: Sim, inner: SimulationInner) -> Self {
        Self { sim, inner: Some(inner) }
    }
}

impl<Sim> Future for Simulation<Sim>
where
    Sim: Future,
{
    type Output = (Sim::Output, SimulationInner);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let out = ready!(this.sim.poll(cx));
        let inner = this.inner.take().expect("Simulation polled after completion");
        Poll::Ready((out, inner))
    }
}

#[derive(Debug)]
struct SimulationInner {
    retries: usize,
    /// The request object that was used for simulation.
    request: SendBundleRequest,
    /// The overrides that were used for simulation.
    overrides: SimBundleOverrides,
}

/// Error thrown when adding a simulation fails.
#[derive(Debug, thiserror::Error)]
pub enum AddSimulationError {
    /// Thrown when too many jobs are queued.
    #[error("too many jobs: {0}")]
    TooManyJobs(u64),
}

/// Message type passed to [BundleSimulatorService].
enum BundleSimulatorMessage {
    /// Clear all ongoing jobs.
    ClearJobs { res: oneshot::Sender<()> },
    /// Set current block number
    UpdateBlockNumber(u64),
    /// Add a new simulation job
    AddSimulation { request: SimulationRequest, tx: oneshot::Sender<()> },
    /// Queues in a new event listener.
    AddEventListener(mpsc::UnboundedSender<SimulationEvent>),
}

/// Events emitted by the simulation service.
#[derive(Debug, Clone)]
pub enum SimulationEvent {
    /// Result of a simulated bundle.
    SimulatedBundle(Result<SimulatedBundle, SimulatedBundleError>),
    /// Updated block number
    BlockNumberUpdated {
        /// replaced block number.
        old: u64,
        /// new block number.
        current: u64,
    },
}

pin_project! {
    /// A stream that yields simulation events.
    pub struct SimulationEventStream {
        #[pin]
        inner: mpsc::UnboundedReceiver<SimulationEvent>,
    }
}

impl SimulationEventStream {
    /// Only yields simulation results
    pub fn results(self) -> SimulationResultStream {
        SimulationResultStream { inner: self.inner }
    }
}

impl Stream for SimulationEventStream {
    type Item = SimulationEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_recv(cx)
    }
}

pin_project! {
    /// A stream that yields outcome of simulations.
    pub struct SimulationResultStream {
        #[pin]
        inner: mpsc::UnboundedReceiver<SimulationEvent>,
    }
}

impl Stream for SimulationResultStream {
    type Item = Result<SimulatedBundle, SimulatedBundleError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            return match ready!(self.as_mut().project().inner.poll_recv(cx)) {
                None => Poll::Ready(None),
                Some(SimulationEvent::SimulatedBundle(res)) => Poll::Ready(Some(res)),
                _ => continue,
            }
        }
    }
}

/// Failed to simulate a bundle.
#[derive(Debug, Clone)]
pub struct SimulatedBundleError {
    // TODO: more error cases? enum?
    inner: Arc<SimulatedBundleErrorInner>,
}

#[derive(Debug)]
struct SimulatedBundleErrorInner {
    error: Box<dyn Error + Send + Sync>,
    sim: SimulationInner,
}

/// How to queue in a simulation.
#[derive(Debug, Clone, Default)]
pub enum SimulationPriority {
    /// The simulation is not urgent.
    #[default]
    Normal,
    /// The simulation should be prioritized.
    High,
}

struct SimulationRequest {
    /// The request object that was used for simulation.
    request: SendBundleRequest,
    /// The overrides that were used for simulation.
    overrides: SimBundleOverrides,
    /// The priority of the simulation.
    priority: SimulationPriority,
}

/// A simulated bundle.
#[derive(Debug, Clone)]
pub struct SimulatedBundle {
    /// The request object that was used for simulation.
    pub request: SendBundleRequest,
    /// The overrides that were used for simulation.
    pub overrides: SimBundleOverrides,
    /// The response from the simulation.simulation
    pub response: SimBundleResponse,
    /// The number of retries that were used for simulation.
    pub retries: usize,
}

impl SimulatedBundle {
    /// Returns true if the simulation was successful.
    pub fn is_success(&self) -> bool {
        self.response.success
    }

    /// Returns the profit of the simulation.
    pub fn profit(&self) -> u64 {
        self.response.profit.as_u64()
    }

    /// Returns the gas used by the simulation.
    pub fn gas_used(&self) -> u64 {
        self.response.gas_used.as_u64()
    }

    /// Returns the mev gas price of the simulation.
    pub fn mev_gas_price(&self) -> u64 {
        self.response.mev_gas_price.as_u64()
    }
}
