//! Transport-independent, metadata-only endpoint supervision.
//!
//! `Supervisor::next_update` is the sole registry writer; drive it in the UI's
//! select loop. Connecting, reading, health checks, and retry waits run in one
//! independent task per enabled machine. No task owns an interactive connection.
//! Adapters must negotiate a control connection, subscribe to full metadata, and
//! classify failures before returning them here. Dropping an adapter or an
//! in-flight connect future must release its transport (including any SSH child).
//!
//! This module deliberately does not accept `ServerMessage`: screen frames and
//! terminal input cannot enter the metadata registry. Agent summaries are already
//! part of `ResourceSnapshot`.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::MissedTickBehavior,
};
use uuid::Uuid;

use crate::{
    alerts::ClientAlertSnapshot,
    machines::Machine,
    protocol::{ClientPresenceSnapshot, ExtensionCatalog, remote::Capabilities},
    resources::ResourceSnapshot,
};

pub(crate) type Async<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum MachineId {
    Local,
    Ssh(Uuid),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EndpointSpec {
    Local,
    Ssh(Machine),
}

impl EndpointSpec {
    pub(crate) fn id(&self) -> MachineId {
        match self {
            Self::Local => MachineId::Local,
            Self::Ssh(machine) => MachineId::Ssh(machine.id),
        }
    }

    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Local => "Local",
            Self::Ssh(machine) => &machine.label,
        }
    }

    fn enabled(&self) -> bool {
        match self {
            Self::Local => true,
            Self::Ssh(machine) => machine.enabled,
        }
    }

    fn same_target(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local, Self::Local) => true,
            (Self::Ssh(a), Self::Ssh(b)) => a.target == b.target,
            _ => false,
        }
    }
}

/// Both counters are process-wide, monotonically increasing, and never reused,
/// including after removal/re-addition or replacement of the entire supervisor.
/// Connection zero means that a worker has not started its first attempt yet.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Generation {
    pub(crate) supervisor: u64,
    pub(crate) connection: u64,
}

static SUPERVISOR_GENERATION: AtomicU64 = AtomicU64::new(1);
static CONNECTION_GENERATION: AtomicU64 = AtomicU64::new(1);

fn allocate(counter: &AtomicU64) -> u64 {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .expect("endpoint generation exhausted")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Status {
    Connecting,
    Ready,
    Reconnecting,
    Attention,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FailureKind {
    Transient,
    HostKey,
    Authentication,
    Installation,
    Compatibility,
}

/// Only transient failures retry automatically. Other failures require an
/// explicit `restart` after the user has repaired the endpoint interactively.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Failure {
    pub(crate) kind: FailureKind,
    pub(crate) message: String,
}

impl Failure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Transient,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Negotiation {
    Local {
        protocol_generation: u16,
    },
    Ssh {
        protocol_generation: u16,
        server_version: String,
        capabilities: Capabilities,
    },
}

/// Full initial resource/presence snapshots are required before becoming Ready.
/// Optional streams stay absent when the peer did not negotiate them.
pub(crate) struct InitialMetadata {
    pub(crate) negotiation: Negotiation,
    pub(crate) resources: ResourceSnapshot,
    pub(crate) presence: ClientPresenceSnapshot,
    pub(crate) alerts: Option<ClientAlertSnapshot>,
    pub(crate) catalog: Option<ExtensionCatalog>,
}

pub(crate) enum Update {
    Resources(ResourceSnapshot),
    Presence(ClientPresenceSnapshot),
    Alerts(ClientAlertSnapshot),
    Catalog(ExtensionCatalog),
    /// A response to the most recent application-level ping, not just traffic.
    Healthy,
}

pub(crate) struct Connected {
    pub(crate) initial: InitialMetadata,
    pub(crate) connection: Box<dyn Connection>,
}

pub(crate) trait Connector: Send + Sync + 'static {
    /// Includes negotiation and the initial full snapshots. A timeout, restart,
    /// disable, removal, or shutdown may cancel this future at any await point.
    fn connect(&self, spec: EndpointSpec) -> Async<'_, Result<Connected, Failure>>;
}

pub(crate) trait Connection: Send + 'static {
    /// Must be cancellation safe: health checks can cancel a pending read.
    /// `None` is EOF. Each metadata update replaces one complete scoped snapshot.
    fn next(&mut self) -> Async<'_, Result<Option<Update>, Failure>>;

    /// Start an application-level probe without consuming metadata. Adapters use
    /// Ping/Pong when negotiated and ListResources/Resources as the fallback.
    /// `next` subsequently yields Healthy only for the correlated response.
    fn ping(&mut self) -> Async<'_, Result<(), Failure>>;
}

/// Injected time makes all deadlines and retry waits deterministic in tests.
pub(crate) trait Clock: Send + Sync + 'static {
    fn sleep(&self, duration: Duration) -> Async<'static, ()>;
}

struct TokioClock;

impl Clock for TokioClock {
    fn sleep(&self, duration: Duration) -> Async<'static, ()> {
        Box::pin(tokio::time::sleep(duration))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) retry_initial: Duration,
    pub(crate) retry_max: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) health_interval: Duration,
    pub(crate) health_timeout: Duration,
    pub(crate) event_capacity: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            retry_initial: Duration::from_secs(1),
            retry_max: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(15),
            health_interval: Duration::from_secs(15),
            health_timeout: Duration::from_secs(5),
            event_capacity: 128,
        }
    }
}

impl Config {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.event_capacity > 0, "event capacity must be positive");
        anyhow::ensure!(
            !self.retry_initial.is_zero() && self.retry_initial <= self.retry_max,
            "retry bounds must be positive and ordered"
        );
        anyhow::ensure!(
            !self.connect_timeout.is_zero()
                && !self.health_interval.is_zero()
                && !self.health_timeout.is_zero(),
            "connection and health deadlines must be positive"
        );
        Ok(())
    }

    fn retry_delay(&self, failures: u32) -> Duration {
        let mut delay = self.retry_initial;
        for _ in 0..failures {
            delay = delay.saturating_mul(2).min(self.retry_max);
            if delay == self.retry_max {
                break;
            }
        }
        delay
    }
}

/// Provenance is retained with each snapshot. A new connection can be Ready
/// while a previously supported optional stream is still stale.
#[derive(Clone, Debug)]
pub(crate) struct Snapshot<T> {
    pub(crate) generation: Generation,
    pub(crate) value: T,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Metadata {
    pub(crate) resources: Option<Snapshot<ResourceSnapshot>>,
    pub(crate) presence: Option<Snapshot<ClientPresenceSnapshot>>,
    pub(crate) alerts: Option<Snapshot<ClientAlertSnapshot>>,
    pub(crate) catalog: Option<Snapshot<ExtensionCatalog>>,
}

#[derive(Clone, Debug)]
pub(crate) struct Endpoint {
    pub(crate) spec: EndpointSpec,
    pub(crate) generation: Generation,
    pub(crate) status: Status,
    pub(crate) negotiation: Option<Snapshot<Negotiation>>,
    pub(crate) metadata: Metadata,
    pub(crate) failure: Option<Failure>,
    accepting_events: bool,
}

impl Endpoint {
    pub(crate) fn is_current<T>(&self, snapshot: &Snapshot<T>) -> bool {
        self.status == Status::Ready && snapshot.generation == self.generation
    }

    pub(crate) fn is_stale(&self) -> bool {
        self.status != Status::Ready
    }
}

/// Read-only machine-scoped state; active identity is independent of membership.
/// Removing the active machine leaves its ID selected and `active_ready` empty.
/// Only `select_active` can change the active ID, never reconnect/reconciliation.
#[derive(Clone)]
pub(crate) struct Registry {
    endpoints: BTreeMap<MachineId, Endpoint>,
    active: MachineId,
}

impl Registry {
    pub(crate) fn get(&self, id: MachineId) -> Option<&Endpoint> {
        self.endpoints.get(&id)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (MachineId, &Endpoint)> {
        self.endpoints.iter().map(|(&id, endpoint)| (id, endpoint))
    }

    pub(crate) fn active(&self) -> MachineId {
        self.active
    }

    pub(crate) fn active_ready(&self) -> Option<&Endpoint> {
        self.get(self.active)
            .filter(|endpoint| !endpoint.is_stale())
    }

    fn apply(&mut self, event: Event) -> bool {
        let Some(endpoint) = self.endpoints.get_mut(&event.id) else {
            return false;
        };
        if endpoint.status == Status::Disabled
            || endpoint.generation.supervisor != event.generation.supervisor
        {
            return false;
        }
        if let EventKind::Attempt = event.kind {
            if event.generation.connection <= endpoint.generation.connection
                || !matches!(endpoint.status, Status::Connecting | Status::Reconnecting)
            {
                return false;
            }
            endpoint.generation = event.generation;
            endpoint.accepting_events = true;
            return true;
        }
        if event.generation != endpoint.generation || !endpoint.accepting_events {
            return false;
        }
        match event.kind {
            EventKind::Attempt => unreachable!(),
            EventKind::Connected(initial) => {
                if !matches!(endpoint.status, Status::Connecting | Status::Reconnecting) {
                    return false;
                }
                endpoint.status = Status::Ready;
                endpoint.failure = None;
                endpoint.negotiation = Some(Snapshot {
                    generation: event.generation,
                    value: initial.negotiation,
                });
                let metadata = &mut endpoint.metadata;
                replace(&mut metadata.resources, initial.resources, event.generation);
                replace(&mut metadata.presence, initial.presence, event.generation);
                if let Some(alerts) = initial.alerts {
                    replace(&mut metadata.alerts, alerts, event.generation);
                }
                if let Some(catalog) = initial.catalog {
                    replace(&mut metadata.catalog, catalog, event.generation);
                }
                true
            }
            EventKind::Update(update) => {
                if endpoint.status != Status::Ready {
                    return false;
                }
                let metadata = &mut endpoint.metadata;
                match update {
                    Update::Resources(value) => {
                        accept(&mut metadata.resources, value, event.generation, |v| {
                            v.revision
                        })
                    }
                    Update::Presence(value) => {
                        accept(&mut metadata.presence, value, event.generation, |v| {
                            v.revision
                        })
                    }
                    Update::Alerts(value) => {
                        accept(&mut metadata.alerts, value, event.generation, |v| {
                            v.revision
                        })
                    }
                    Update::Catalog(value) => {
                        accept(&mut metadata.catalog, value, event.generation, |v| {
                            v.generation
                        })
                    }
                    Update::Healthy => false,
                }
            }
            EventKind::Failed(failure) => {
                if endpoint.status == Status::Attention {
                    return false;
                }
                endpoint.status = if failure.kind == FailureKind::Transient {
                    Status::Reconnecting
                } else {
                    Status::Attention
                };
                endpoint.accepting_events = false;
                endpoint.failure = Some(failure);
                true
            }
        }
    }
}

fn replace<T>(slot: &mut Option<Snapshot<T>>, value: T, generation: Generation) {
    *slot = Some(Snapshot { generation, value });
}

fn accept<T>(
    slot: &mut Option<Snapshot<T>>,
    value: T,
    generation: Generation,
    revision: impl Fn(&T) -> u64,
) -> bool {
    if slot
        .as_ref()
        .is_some_and(|old| old.generation == generation && revision(&old.value) >= revision(&value))
    {
        return false;
    }
    replace(slot, value, generation);
    true
}

struct Event {
    id: MachineId,
    generation: Generation,
    kind: EventKind,
}

enum EventKind {
    Attempt,
    Connected(InitialMetadata),
    Update(Update),
    Failed(Failure),
}

pub(crate) struct Supervisor {
    registry: Registry,
    connector: Arc<dyn Connector>,
    clock: Arc<dyn Clock>,
    config: Config,
    sender: mpsc::Sender<Event>,
    receiver: mpsc::Receiver<Event>,
    workers: BTreeMap<MachineId, JoinHandle<()>>,
    retired: Vec<JoinHandle<()>>,
}

#[derive(Clone)]
pub(crate) struct State {
    pub(crate) registry: Registry,
    pub(crate) catalog_error: Option<String>,
}

#[cfg(test)]
impl State {
    pub(crate) fn fixture(
        endpoints: Vec<(EndpointSpec, Status, Generation, Option<ResourceSnapshot>)>,
    ) -> Self {
        let endpoints = endpoints
            .into_iter()
            .map(|(spec, status, generation, resources)| {
                let id = spec.id();
                (
                    id,
                    Endpoint {
                        spec,
                        generation,
                        status,
                        negotiation: None,
                        metadata: Metadata {
                            resources: resources.map(|value| Snapshot { generation, value }),
                            ..Metadata::default()
                        },
                        failure: None,
                        accepting_events: status == Status::Ready,
                    },
                )
            })
            .collect();
        Self {
            registry: Registry {
                endpoints,
                active: MachineId::Local,
            },
            catalog_error: None,
        }
    }

    pub(crate) fn fixture_metadata_mut(&mut self, machine: MachineId) -> &mut Metadata {
        &mut self.registry.endpoints.get_mut(&machine).unwrap().metadata
    }
}

/// Owns the continuously driven supervisor while today's single-endpoint UI is
/// running. The watch snapshot is the handoff to machine-qualified presentation.
pub(crate) struct Service {
    state: watch::Receiver<Arc<State>>,
    task: JoinHandle<()>,
    stop: Option<oneshot::Sender<()>>,
}

impl Service {
    pub(crate) fn start(
        local_socket: std::path::PathBuf,
        alert_client_id: crate::domain::ClientId,
    ) -> anyhow::Result<Self> {
        let catalog = crate::machines::Catalog::resolve();
        let (catalog_path, machines, catalog_error) = match catalog {
            Ok(catalog) => match catalog.list() {
                Ok(machines) => (Some(catalog.path().to_owned()), machines, None),
                Err(error) => (
                    Some(catalog.path().to_owned()),
                    Vec::new(),
                    Some(format!("{error:#}")),
                ),
            },
            Err(error) => (None, Vec::new(), Some(format!("{error:#}"))),
        };
        let connector = Arc::new(super::federation_transport::SystemConnector::new(
            local_socket,
            alert_client_id,
        ));
        let mut supervisor = Supervisor::new(connector, &machines, Config::default())?;
        debug_assert!(
            supervisor
                .registry()
                .iter()
                .any(|(id, _)| id == MachineId::Local)
        );
        debug_assert!(supervisor.registry().active_ready().is_none());
        debug_assert!(supervisor.select_active(MachineId::Local));
        let initial = Arc::new(State {
            registry: supervisor.registry().clone(),
            catalog_error,
        });
        let (sender, state) = watch::channel(initial);
        let (stop, mut stopping) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut known_machines = machines;
            let mut catalog_error = sender.borrow().catalog_error.clone();
            let mut catalog_poll = tokio::time::interval(Duration::from_secs(1));
            catalog_poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
            catalog_poll.tick().await;
            loop {
                tokio::select! {
                    _ = &mut stopping => break,
                    _ = supervisor.next_update() => {
                        publish_state(&sender, &supervisor, catalog_error.clone());
                    }
                    _ = catalog_poll.tick(), if catalog_path.is_some() => {
                        let path = catalog_path.clone().expect("guarded catalog path");
                        match tokio::task::spawn_blocking(move || crate::machines::Catalog::at(path).list()).await {
                            Ok(Ok(machines)) => {
                                catalog_error = None;
                                if machines != known_machines {
                                    if let Err(error) = supervisor.reconcile(&machines) {
                                        catalog_error = Some(format!("{error:#}"));
                                    } else {
                                        known_machines = machines;
                                    }
                                }
                            }
                            Ok(Err(error)) => catalog_error = Some(format!("{error:#}")),
                            Err(error) => catalog_error = Some(format!("machine catalog reader failed: {error}")),
                        }
                        publish_state(&sender, &supervisor, catalog_error.clone());
                    }
                }
            }
            supervisor.shutdown().await;
        });
        Ok(Self {
            state,
            task,
            stop: Some(stop),
        })
    }

    pub(crate) fn snapshot(&self) -> Arc<State> {
        self.state.borrow().clone()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Arc<State>> {
        self.state.clone()
    }

    pub(crate) async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = (&mut self.task).await;
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        // Error paths cannot await, but they must synchronously drop every
        // transport owner instead of leaving background SSH alive until the
        // surrounding runtime happens to stop.
        self.task.abort();
    }
}

fn publish_state(
    sender: &watch::Sender<Arc<State>>,
    supervisor: &Supervisor,
    catalog_error: Option<String>,
) {
    sender.send_replace(Arc::new(State {
        registry: supervisor.registry().clone(),
        catalog_error,
    }));
}

impl Supervisor {
    /// Must be constructed inside a Tokio runtime. Starts Local plus every
    /// enabled saved machine without awaiting any connection. Disabled saved
    /// profiles remain visible but never get a worker.
    pub(crate) fn new(
        connector: Arc<dyn Connector>,
        machines: &[Machine],
        config: Config,
    ) -> anyhow::Result<Self> {
        Self::with_clock(connector, machines, config, Arc::new(TokioClock))
    }

    pub(crate) fn with_clock(
        connector: Arc<dyn Connector>,
        machines: &[Machine],
        config: Config,
        clock: Arc<dyn Clock>,
    ) -> anyhow::Result<Self> {
        config.validate()?;
        tokio::runtime::Handle::try_current()?;
        let (sender, receiver) = mpsc::channel(config.event_capacity);
        let mut supervisor = Self {
            registry: Registry {
                endpoints: BTreeMap::new(),
                active: MachineId::Local,
            },
            connector,
            clock,
            config,
            sender,
            receiver,
            workers: BTreeMap::new(),
            retired: Vec::new(),
        };
        supervisor.reconcile(machines)?;
        Ok(supervisor)
    }

    pub(crate) fn registry(&self) -> &Registry {
        &self.registry
    }

    /// May explicitly select a disabled/unavailable saved profile. Returns false
    /// for an unknown identity; it never silently falls back to Local.
    pub(crate) fn select_active(&mut self, id: MachineId) -> bool {
        if !self.registry.endpoints.contains_key(&id) {
            return false;
        }
        self.registry.active = id;
        true
    }

    /// Apply a complete, already validated saved-machine catalog. Duplicate UUIDs
    /// reject the entire reconciliation before mutating anything. Labels are
    /// presentation only; changing a target/enabled flag replaces the worker.
    pub(crate) fn reconcile(&mut self, machines: &[Machine]) -> anyhow::Result<()> {
        let mut desired = BTreeMap::from([(MachineId::Local, EndpointSpec::Local)]);
        for machine in machines {
            let spec = EndpointSpec::Ssh(machine.clone());
            anyhow::ensure!(
                desired.insert(spec.id(), spec).is_none(),
                "duplicate saved machine UUID {}",
                machine.id
            );
        }
        let removed: BTreeSet<_> = self
            .registry
            .endpoints
            .keys()
            .filter(|id| !desired.contains_key(id))
            .copied()
            .collect();
        for id in removed {
            self.stop(id);
            self.registry.endpoints.remove(&id);
        }
        for (id, spec) in desired {
            let target_changed = self
                .registry
                .get(id)
                .is_some_and(|old| !old.spec.same_target(&spec));
            let restart = self.registry.get(id).is_none_or(|old| {
                old.spec.enabled() != spec.enabled() || !old.spec.same_target(&spec)
            });
            if let Some(endpoint) = self.registry.endpoints.get_mut(&id) {
                endpoint.spec = spec.clone();
                if target_changed {
                    // A stable profile ID must not make the previous host's
                    // metadata look authoritative for a replacement target.
                    endpoint.negotiation = None;
                    endpoint.metadata = Metadata::default();
                }
            } else {
                self.registry.endpoints.insert(
                    id,
                    Endpoint {
                        spec,
                        generation: Generation::default(),
                        status: Status::Connecting,
                        negotiation: None,
                        metadata: Metadata::default(),
                        failure: None,
                        accepting_events: false,
                    },
                );
            }
            if restart {
                self.replace_worker(id);
            }
        }
        Ok(())
    }

    /// Explicitly retry an Attention endpoint (or reconnect a live one).
    #[cfg(test)]
    pub(crate) fn restart(&mut self, id: MachineId) -> bool {
        if !self.registry.get(id).is_some_and(|e| e.spec.enabled()) {
            return false;
        }
        self.replace_worker(id);
        true
    }

    fn stop(&mut self, id: MachineId) {
        if let Some(worker) = self.workers.remove(&id) {
            worker.abort();
            self.retired.retain(|worker| !worker.is_finished());
            self.retired.push(worker);
        }
    }

    fn replace_worker(&mut self, id: MachineId) {
        self.stop(id);
        let endpoint = self
            .registry
            .endpoints
            .get_mut(&id)
            .expect("known endpoint");
        endpoint.generation.supervisor = allocate(&SUPERVISOR_GENERATION);
        // Preserve the connection watermark across target changes and disable.
        // The new worker will allocate a strictly higher connection generation.
        endpoint.failure = None;
        endpoint.accepting_events = false;
        if !endpoint.spec.enabled() {
            endpoint.status = Status::Disabled;
            return;
        }
        endpoint.status = if endpoint.metadata.resources.is_some() {
            Status::Reconnecting
        } else {
            Status::Connecting
        };
        self.workers.insert(
            id,
            tokio::spawn(run_worker(
                endpoint.spec.clone(),
                endpoint.generation.supervisor,
                self.connector.clone(),
                self.clock.clone(),
                self.config.clone(),
                self.sender.clone(),
            )),
        );
    }

    /// Wait for one accepted state change, returning the affected machine.
    /// Cancellation safe. Rejected stale events never produce a notification.
    /// Registry changes from reconcile/select_active/restart are synchronous.
    pub(crate) async fn next_update(&mut self) -> MachineId {
        loop {
            let event = self.receiver.recv().await.expect("supervisor owns sender");
            let id = event.id;
            if self.registry.apply(event) {
                return id;
            }
        }
    }

    /// Abort and join all workers, including stalled connection attempts. Drop
    /// also aborts, but cannot await transport destruction; use this at shutdown.
    pub(crate) async fn shutdown(mut self) {
        for worker in self.workers.values() {
            worker.abort();
        }
        for (_, worker) in std::mem::take(&mut self.workers) {
            let _ = worker.await;
        }
        for worker in std::mem::take(&mut self.retired) {
            let _ = worker.await;
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        for worker in self.workers.values() {
            worker.abort();
        }
    }
}

async fn deadline<T>(
    clock: &dyn Clock,
    duration: Duration,
    operation: Async<'_, Result<T, Failure>>,
    message: &'static str,
) -> Result<T, Failure> {
    tokio::select! {
        result = operation => result,
        () = clock.sleep(duration) => Err(Failure::transient(message)),
    }
}

async fn run_worker(
    spec: EndpointSpec,
    supervisor: u64,
    connector: Arc<dyn Connector>,
    clock: Arc<dyn Clock>,
    config: Config,
    sender: mpsc::Sender<Event>,
) {
    let id = spec.id();
    let mut failures = 0_u32;
    loop {
        let generation = Generation {
            supervisor,
            connection: allocate(&CONNECTION_GENERATION),
        };
        let send = |kind| {
            sender.send(Event {
                id,
                generation,
                kind,
            })
        };
        if send(EventKind::Attempt).await.is_err() {
            return;
        }
        let result = deadline(
            clock.as_ref(),
            config.connect_timeout,
            connector.connect(spec.clone()),
            "endpoint connection timed out",
        )
        .await;
        let failure = match result {
            Err(failure) => failure,
            Ok(Connected {
                initial,
                mut connection,
            }) => {
                if send(EventKind::Connected(initial)).await.is_err() {
                    return;
                }
                let failure = read_connection(
                    connection.as_mut(),
                    clock.as_ref(),
                    &config,
                    &sender,
                    id,
                    generation,
                    &mut failures,
                )
                .await;
                // Drop the transport before notifying the registry or retrying.
                drop(connection);
                let Some(failure) = failure else { return };
                failure
            }
        };
        let retry = failure.kind == FailureKind::Transient;
        if send(EventKind::Failed(failure)).await.is_err() || !retry {
            return;
        }
        clock.sleep(config.retry_delay(failures)).await;
        failures = failures.saturating_add(1);
    }
}

#[allow(clippy::too_many_arguments)]
async fn read_connection(
    connection: &mut dyn Connection,
    clock: &dyn Clock,
    config: &Config,
    sender: &mpsc::Sender<Event>,
    id: MachineId,
    generation: Generation,
    failures: &mut u32,
) -> Option<Failure> {
    let mut timer = clock.sleep(config.health_interval);
    let mut awaiting_pong = false;
    loop {
        tokio::select! {
            update = connection.next() => {
                match update {
                    Err(failure) => return Some(failure),
                    Ok(None) => return Some(Failure::transient("endpoint disconnected (EOF)")),
                    Ok(Some(Update::Healthy)) => {
                        if awaiting_pong {
                            awaiting_pong = false;
                            timer = clock.sleep(config.health_interval);
                            // A completed handshake alone must not reset backoff:
                            // otherwise immediate-EOF peers can cause retry storms.
                            *failures = 0;
                        }
                    }
                    Ok(Some(update)) => {
                        if sender.send(Event {
                            id, generation, kind: EventKind::Update(update),
                        }).await.is_err() {
                            return None;
                        }
                    }
                }
            }
            () = &mut timer => {
                if awaiting_pong {
                    return Some(Failure::transient("endpoint health check timed out"));
                }
                if let Err(failure) = deadline(
                    clock, config.health_timeout, connection.ping(),
                    "endpoint ping timed out",
                ).await {
                    return Some(failure);
                }
                awaiting_pong = true;
                timer = clock.sleep(config.health_timeout);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    type Answer = oneshot::Sender<Result<Connected, Failure>>;

    struct Attempt {
        spec: EndpointSpec,
        answer: Answer,
    }

    struct FakeConnector(mpsc::UnboundedSender<Attempt>);

    impl Connector for FakeConnector {
        fn connect(&self, spec: EndpointSpec) -> Async<'_, Result<Connected, Failure>> {
            Box::pin(async move {
                let (answer, response) = oneshot::channel();
                self.0.send(Attempt { spec, answer }).unwrap();
                response.await.unwrap()
            })
        }
    }

    struct Sleep {
        duration: Duration,
        wake: oneshot::Sender<()>,
    }

    struct FakeClock(mpsc::UnboundedSender<Sleep>);

    impl Clock for FakeClock {
        fn sleep(&self, duration: Duration) -> Async<'static, ()> {
            let sender = self.0.clone();
            Box::pin(async move {
                let (wake, wait) = oneshot::channel();
                sender.send(Sleep { duration, wake }).unwrap();
                let _ = wait.await;
            })
        }
    }

    struct Timers {
        receiver: mpsc::UnboundedReceiver<Sleep>,
        pending: Vec<Sleep>,
    }

    impl Timers {
        async fn fire(&mut self, duration: Duration) {
            guard(async {
                loop {
                    self.pending.retain(|sleep| !sleep.wake.is_closed());
                    if let Some(index) = self.pending.iter().position(|s| s.duration == duration) {
                        if self.pending.remove(index).wake.send(()).is_ok() {
                            return;
                        }
                    } else {
                        self.pending.push(self.receiver.recv().await.unwrap());
                    }
                }
            })
            .await;
        }
    }

    struct FakeConnection {
        updates: mpsc::UnboundedReceiver<Result<Update, Failure>>,
        pings: mpsc::UnboundedSender<()>,
        dropped: Arc<AtomicU64>,
    }

    impl Drop for FakeConnection {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Connection for FakeConnection {
        fn next(&mut self) -> Async<'_, Result<Option<Update>, Failure>> {
            Box::pin(async { self.updates.recv().await.transpose() })
        }

        fn ping(&mut self) -> Async<'_, Result<(), Failure>> {
            Box::pin(async {
                self.pings.send(()).unwrap();
                Ok(())
            })
        }
    }

    struct Live {
        updates: mpsc::UnboundedSender<Result<Update, Failure>>,
        pings: mpsc::UnboundedReceiver<()>,
        dropped: Arc<AtomicU64>,
    }

    fn connected(initial: InitialMetadata) -> (Connected, Live) {
        let (updates, receiver) = mpsc::unbounded_channel();
        let (pings, ping_receiver) = mpsc::unbounded_channel();
        let dropped = Arc::new(AtomicU64::new(0));
        (
            Connected {
                initial,
                connection: Box::new(FakeConnection {
                    updates: receiver,
                    pings,
                    dropped: dropped.clone(),
                }),
            },
            Live {
                updates,
                pings: ping_receiver,
                dropped,
            },
        )
    }

    fn initial(revision: u64) -> InitialMetadata {
        InitialMetadata {
            negotiation: Negotiation::Local {
                protocol_generation: crate::protocol::local::GENERATION,
            },
            resources: resources(revision),
            presence: ClientPresenceSnapshot {
                revision,
                sessions: Vec::new(),
            },
            alerts: Some(ClientAlertSnapshot {
                revision,
                terminals: Vec::new(),
            }),
            catalog: Some(catalog(revision)),
        }
    }

    fn resources(revision: u64) -> ResourceSnapshot {
        ResourceSnapshot {
            revision,
            sessions: Vec::new(),
        }
    }

    fn catalog(generation: u64) -> ExtensionCatalog {
        ExtensionCatalog {
            generation,
            fingerprint: format!("catalog-{generation}"),
            extensions: Vec::new(),
            config: Default::default(),
        }
    }

    fn machine(n: u128) -> Machine {
        Machine {
            id: Uuid::from_u128(n),
            label: format!("machine-{n}"),
            target: format!("host-{n}"),
            enabled: true,
        }
    }

    fn config() -> Config {
        Config {
            retry_initial: Duration::from_secs(1),
            retry_max: Duration::from_secs(8),
            connect_timeout: Duration::from_secs(101),
            health_interval: Duration::from_secs(103),
            health_timeout: Duration::from_secs(107),
            event_capacity: 16,
        }
    }

    fn setup(machines: &[Machine]) -> (Supervisor, mpsc::UnboundedReceiver<Attempt>, Timers) {
        let (sender, attempts) = mpsc::unbounded_channel();
        let (sleeps, receiver) = mpsc::unbounded_channel();
        let supervisor = Supervisor::with_clock(
            Arc::new(FakeConnector(sender)),
            machines,
            config(),
            Arc::new(FakeClock(sleeps)),
        )
        .unwrap();
        (
            supervisor,
            attempts,
            Timers {
                receiver,
                pending: Vec::new(),
            },
        )
    }

    async fn guard<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("test made no progress")
    }

    async fn status(supervisor: &mut Supervisor, id: MachineId, expected: Status) {
        guard(async {
            while supervisor.registry().get(id).unwrap().status != expected {
                supervisor.next_update().await;
            }
        })
        .await;
    }

    async fn ready(supervisor: &mut Supervisor, attempt: Attempt, revision: u64) -> Live {
        let id = attempt.spec.id();
        let (connection, live) = connected(initial(revision));
        assert!(attempt.answer.send(Ok(connection)).is_ok());
        status(supervisor, id, Status::Ready).await;
        live
    }

    fn apply(
        supervisor: &mut Supervisor,
        id: MachineId,
        generation: Generation,
        kind: EventKind,
    ) -> bool {
        supervisor.registry.apply(Event {
            id,
            generation,
            kind,
        })
    }

    #[tokio::test]
    async fn partial_startup_and_stalled_endpoint_do_not_block_other_machines() {
        let remote = machine(1);
        let mut disabled = machine(2);
        disabled.enabled = false;
        let remote_id = MachineId::Ssh(remote.id);
        let (mut supervisor, mut attempts, _timers) = setup(&[remote, disabled.clone()]);
        let mut started = BTreeMap::new();
        for _ in 0..2 {
            let attempt = guard(attempts.recv()).await.unwrap();
            started.insert(attempt.spec.id(), attempt);
        }
        let stalled = started.remove(&remote_id).unwrap();
        let local = ready(
            &mut supervisor,
            started.remove(&MachineId::Local).unwrap(),
            40,
        )
        .await;
        assert!(attempts.try_recv().is_err());
        assert_eq!(supervisor.registry().iter().count(), 3);
        assert_eq!(
            supervisor.registry().get(remote_id).unwrap().status,
            Status::Connecting
        );
        assert_eq!(
            supervisor
                .registry()
                .get(MachineId::Ssh(disabled.id))
                .unwrap()
                .status,
            Status::Disabled
        );
        local
            .updates
            .send(Ok(Update::Resources(resources(41))))
            .unwrap();
        guard(supervisor.next_update()).await;
        assert_eq!(
            supervisor
                .registry()
                .active_ready()
                .unwrap()
                .metadata
                .resources
                .as_ref()
                .unwrap()
                .value
                .revision,
            41
        );
        let remote = ready(&mut supervisor, stalled, 3).await;
        assert_eq!(
            supervisor
                .registry()
                .get(remote_id)
                .unwrap()
                .metadata
                .resources
                .as_ref()
                .unwrap()
                .value
                .revision,
            3
        );
        assert_eq!(supervisor.registry().active(), MachineId::Local);
        supervisor.shutdown().await;
        assert_eq!(local.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(remote.dropped.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn two_remote_endpoints_fail_reconnect_and_cleanup_independently() {
        let alpha = machine(11);
        let beta = machine(12);
        let alpha_id = MachineId::Ssh(alpha.id);
        let beta_id = MachineId::Ssh(beta.id);
        let (mut supervisor, mut attempts, mut timers) = setup(&[alpha, beta]);
        let mut started = BTreeMap::new();
        for _ in 0..3 {
            let attempt = guard(attempts.recv()).await.unwrap();
            started.insert(attempt.spec.id(), attempt);
        }
        let local = ready(
            &mut supervisor,
            started.remove(&MachineId::Local).unwrap(),
            1,
        )
        .await;
        let alpha_live = ready(&mut supervisor, started.remove(&alpha_id).unwrap(), 10).await;
        let beta_live = ready(&mut supervisor, started.remove(&beta_id).unwrap(), 20).await;

        alpha_live
            .updates
            .send(Err(Failure::transient("alpha offline")))
            .unwrap();
        status(&mut supervisor, alpha_id, Status::Reconnecting).await;
        assert_eq!(
            supervisor.registry().get(beta_id).unwrap().status,
            Status::Ready
        );
        assert_eq!(
            supervisor
                .registry()
                .get(beta_id)
                .unwrap()
                .metadata
                .resources
                .as_ref()
                .unwrap()
                .value
                .revision,
            20
        );
        timers.fire(Duration::from_secs(1)).await;
        let retry = guard(attempts.recv()).await.unwrap();
        assert_eq!(retry.spec.id(), alpha_id);
        let alpha_reconnected = ready(&mut supervisor, retry, 2).await;
        assert!(
            supervisor
                .registry()
                .get(alpha_id)
                .unwrap()
                .metadata
                .resources
                .as_ref()
                .is_some_and(|snapshot| snapshot.value.revision == 2)
        );
        assert_eq!(supervisor.registry().active(), MachineId::Local);

        supervisor.shutdown().await;
        assert_eq!(local.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(alpha_live.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(alpha_reconnected.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(beta_live.dropped.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn eof_retains_stale_metadata_and_reconnect_resets_all_revisions() {
        let (mut supervisor, mut attempts, mut timers) = setup(&[]);
        let first = guard(attempts.recv()).await.unwrap();
        let live = ready(&mut supervisor, first, 90).await;
        let id = MachineId::Local;
        let old = supervisor.registry().get(id).unwrap().generation;
        drop(live.updates);
        status(&mut supervisor, id, Status::Reconnecting).await;
        let endpoint = supervisor.registry().get(id).unwrap();
        assert!(endpoint.is_stale());
        assert!(!endpoint.is_current(endpoint.metadata.resources.as_ref().unwrap()));
        assert!(supervisor.registry().active_ready().is_none());
        assert_eq!(
            endpoint.metadata.resources.as_ref().unwrap().value.revision,
            90
        );
        // Even an event tagged with the just-disconnected connection is closed.
        assert!(!apply(
            &mut supervisor,
            id,
            old,
            EventKind::Connected(initial(100))
        ));
        assert!(!apply(
            &mut supervisor,
            id,
            old,
            EventKind::Update(Update::Resources(resources(100)))
        ));
        timers.fire(config().retry_initial).await;
        let second = guard(attempts.recv()).await.unwrap();
        let _live = ready(&mut supervisor, second, 0).await;
        let endpoint = supervisor.registry().get(id).unwrap();
        let new = endpoint.generation;
        assert_eq!(old.supervisor, new.supervisor);
        assert!(new.connection > old.connection);
        assert_eq!(
            endpoint.metadata.resources.as_ref().unwrap().value.revision,
            0
        );
        assert_eq!(
            endpoint.metadata.presence.as_ref().unwrap().value.revision,
            0
        );
        assert_eq!(endpoint.metadata.alerts.as_ref().unwrap().value.revision, 0);
        assert_eq!(
            endpoint.metadata.catalog.as_ref().unwrap().value.generation,
            0
        );
        assert!(endpoint.is_current(endpoint.metadata.resources.as_ref().unwrap()));
        assert!(!apply(
            &mut supervisor,
            id,
            old,
            EventKind::Failed(Failure::transient("late EOF"))
        ));
        assert!(!apply(&mut supervisor, id, old, EventKind::Attempt));
        assert!(!apply(
            &mut supervisor,
            id,
            old,
            EventKind::Update(Update::Catalog(catalog(100)))
        ));
        for update in [
            Update::Resources(resources(1)),
            Update::Presence(ClientPresenceSnapshot {
                revision: 1,
                sessions: Vec::new(),
            }),
            Update::Alerts(ClientAlertSnapshot {
                revision: 1,
                terminals: Vec::new(),
            }),
            Update::Catalog(catalog(1)),
        ] {
            assert!(apply(&mut supervisor, id, new, EventKind::Update(update)));
        }
        for update in [
            Update::Resources(resources(0)),
            Update::Presence(ClientPresenceSnapshot {
                revision: 0,
                sessions: Vec::new(),
            }),
            Update::Alerts(ClientAlertSnapshot {
                revision: 0,
                terminals: Vec::new(),
            }),
            Update::Catalog(catalog(0)),
            Update::Resources(resources(1)),
            Update::Healthy,
        ] {
            assert!(!apply(&mut supervisor, id, new, EventKind::Update(update)));
        }
        assert_eq!(supervisor.registry().active(), id);
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn missing_optional_snapshots_remain_explicitly_stale_after_reconnect() {
        let (mut supervisor, mut attempts, _timers) = setup(&[]);
        let first = guard(attempts.recv()).await.unwrap();
        let _first_live = ready(&mut supervisor, first, 90).await;
        let id = MachineId::Local;
        assert!(supervisor.restart(id));
        let second = guard(attempts.recv()).await.unwrap();
        let mut metadata = initial(0);
        metadata.alerts = None;
        metadata.catalog = None;
        let (connection, _live) = connected(metadata);
        assert!(second.answer.send(Ok(connection)).is_ok());
        status(&mut supervisor, id, Status::Ready).await;
        let endpoint = supervisor.registry().get(id).unwrap();
        assert!(endpoint.is_current(endpoint.metadata.resources.as_ref().unwrap()));
        assert!(!endpoint.is_current(endpoint.metadata.alerts.as_ref().unwrap()));
        assert!(!endpoint.is_current(endpoint.metadata.catalog.as_ref().unwrap()));
        let generation = endpoint.generation;
        assert!(apply(
            &mut supervisor,
            id,
            generation,
            EventKind::Update(Update::Alerts(ClientAlertSnapshot::default()))
        ));
        assert!(apply(
            &mut supervisor,
            id,
            generation,
            EventKind::Update(Update::Catalog(catalog(0)))
        ));
        let endpoint = supervisor.registry().get(id).unwrap();
        assert!(endpoint.is_current(endpoint.metadata.alerts.as_ref().unwrap()));
        assert!(endpoint.is_current(endpoint.metadata.catalog.as_ref().unwrap()));
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn catalog_reconciles_identity_rename_target_disable_remove_and_readd() {
        let mut machine = machine(1);
        let id = MachineId::Ssh(machine.id);
        let (mut supervisor, mut attempts, _timers) = setup(std::slice::from_ref(&machine));
        let mut live = BTreeMap::new();
        for _ in 0..2 {
            let attempt = guard(attempts.recv()).await.unwrap();
            let id = attempt.spec.id();
            live.insert(id, ready(&mut supervisor, attempt, 7).await);
        }
        assert!(supervisor.select_active(id));
        let old = supervisor.registry().get(id).unwrap().generation;
        machine.label = "renamed".into();
        supervisor
            .reconcile(std::slice::from_ref(&machine))
            .unwrap();
        assert_eq!(supervisor.registry().get(id).unwrap().generation, old);
        assert_eq!(
            supervisor.registry().get(id).unwrap().spec,
            EndpointSpec::Ssh(machine.clone())
        );
        assert!(attempts.try_recv().is_err());
        machine.target = "other-target".into();
        supervisor
            .reconcile(std::slice::from_ref(&machine))
            .unwrap();
        let new = supervisor.registry().get(id).unwrap().generation;
        assert!(new.supervisor > old.supervisor);
        assert!(supervisor.registry().active_ready().is_none());
        assert!(
            supervisor
                .registry()
                .get(id)
                .unwrap()
                .metadata
                .resources
                .is_none()
        );
        assert_eq!(supervisor.registry().active(), id);
        assert!(!apply(
            &mut supervisor,
            id,
            old,
            EventKind::Update(Update::Resources(resources(100)))
        ));
        let attempt = guard(attempts.recv()).await.unwrap();
        assert_eq!(attempt.spec, EndpointSpec::Ssh(machine.clone()));
        let remote = ready(&mut supervisor, attempt, 0).await;
        assert_eq!(live[&id].dropped.load(Ordering::Relaxed), 1);
        assert!(supervisor.registry().get(id).unwrap().generation.connection > old.connection);
        let before_disable = supervisor.registry().get(id).unwrap().generation;
        machine.enabled = false;
        supervisor
            .reconcile(std::slice::from_ref(&machine))
            .unwrap();
        assert_eq!(
            supervisor.registry().get(id).unwrap().status,
            Status::Disabled
        );
        assert!(!supervisor.restart(id));
        assert!(!apply(
            &mut supervisor,
            id,
            before_disable,
            EventKind::Connected(initial(100))
        ));
        assert_eq!(supervisor.registry().active(), id);
        supervisor.reconcile(&[]).unwrap();
        assert!(supervisor.registry().get(id).is_none());
        assert_eq!(supervisor.registry().active(), id);
        assert!(supervisor.registry().active_ready().is_none());
        assert!(!apply(
            &mut supervisor,
            id,
            before_disable,
            EventKind::Attempt
        ));
        machine.enabled = true;
        supervisor
            .reconcile(std::slice::from_ref(&machine))
            .unwrap();
        assert_eq!(supervisor.registry().active(), id);
        assert!(
            supervisor
                .registry()
                .get(id)
                .unwrap()
                .metadata
                .resources
                .is_none()
        );
        let attempt = guard(attempts.recv()).await.unwrap();
        let _readded = ready(&mut supervisor, attempt, 0).await;
        assert!(
            supervisor.registry().get(id).unwrap().generation.supervisor
                > before_disable.supervisor
        );
        assert_eq!(remote.dropped.load(Ordering::Relaxed), 1);
        assert!(supervisor.registry().active_ready().is_some());
        assert!(!supervisor.select_active(MachineId::Ssh(Uuid::from_u128(999))));
        assert_eq!(supervisor.registry().active(), id);
        assert!(supervisor.select_active(MachineId::Local));
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn duplicate_catalog_is_rejected_atomically_and_disable_cancels_connect() {
        let machine = machine(1);
        let id = MachineId::Ssh(machine.id);
        let (mut supervisor, mut attempts, _timers) = setup(std::slice::from_ref(&machine));
        let before = supervisor.registry().get(id).unwrap().generation;
        let mut duplicate = machine.clone();
        duplicate.target = "changed".into();
        assert!(supervisor.reconcile(&[machine.clone(), duplicate]).is_err());
        assert_eq!(supervisor.registry().get(id).unwrap().generation, before);
        assert_eq!(
            supervisor.registry().get(id).unwrap().spec,
            EndpointSpec::Ssh(machine.clone())
        );
        let mut pending = Vec::new();
        for _ in 0..2 {
            pending.push(guard(attempts.recv()).await.unwrap());
        }
        let mut disabled = machine;
        disabled.enabled = false;
        supervisor.reconcile(&[disabled]).unwrap();
        let remote = pending.iter_mut().find(|a| a.spec.id() == id).unwrap();
        guard(remote.answer.closed()).await;
        assert_eq!(
            supervisor.registry().get(id).unwrap().status,
            Status::Disabled
        );
        assert!(!apply(
            &mut supervisor,
            id,
            before,
            EventKind::Failed(Failure::transient("late"))
        ));
        supervisor.shutdown().await;
        for attempt in pending {
            assert!(attempt.answer.is_closed());
        }
    }

    #[tokio::test]
    async fn connect_timeout_and_immediate_eof_backoff_are_capped() {
        let (mut supervisor, mut attempts, mut timers) = setup(&[]);
        let mut stalled = guard(attempts.recv()).await.unwrap();
        timers.fire(config().connect_timeout).await;
        status(&mut supervisor, MachineId::Local, Status::Reconnecting).await;
        guard(stalled.answer.closed()).await;
        let mut previous = supervisor
            .registry()
            .get(MachineId::Local)
            .unwrap()
            .generation
            .connection;
        for seconds in [1, 2, 4, 8, 8] {
            timers.fire(Duration::from_secs(seconds)).await;
            let attempt = guard(attempts.recv()).await.unwrap();
            let live = ready(&mut supervisor, attempt, 0).await;
            let generation = supervisor
                .registry()
                .get(MachineId::Local)
                .unwrap()
                .generation
                .connection;
            assert!(generation > previous);
            previous = generation;
            drop(live.updates);
            status(&mut supervisor, MachineId::Local, Status::Reconnecting).await;
        }
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn attention_failures_require_explicit_restart_and_preserve_active_identity() {
        let (mut supervisor, mut attempts, _timers) = setup(&[]);
        for kind in [
            FailureKind::HostKey,
            FailureKind::Authentication,
            FailureKind::Installation,
            FailureKind::Compatibility,
        ] {
            let attempt = guard(attempts.recv()).await.unwrap();
            assert!(
                attempt
                    .answer
                    .send(Err(Failure {
                        kind,
                        message: "repair interactively".into()
                    }))
                    .is_ok()
            );
            status(&mut supervisor, MachineId::Local, Status::Attention).await;
            let endpoint = supervisor.registry().get(MachineId::Local).unwrap();
            assert_eq!(endpoint.failure.as_ref().unwrap().kind, kind);
            assert!(endpoint.is_stale());
            assert!(attempts.try_recv().is_err());
            assert_eq!(supervisor.registry().active(), MachineId::Local);
            assert!(supervisor.restart(MachineId::Local));
        }
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn health_deadline_survives_metadata_traffic_and_pong_resets_backoff() {
        let (mut supervisor, mut attempts, mut timers) = setup(&[]);
        let attempt = guard(attempts.recv()).await.unwrap();
        assert!(
            attempt
                .answer
                .send(Err(Failure::transient("offline")))
                .is_ok()
        );
        status(&mut supervisor, MachineId::Local, Status::Reconnecting).await;
        timers.fire(config().retry_initial).await;
        let attempt = guard(attempts.recv()).await.unwrap();
        let mut live = ready(&mut supervisor, attempt, 0).await;
        timers.fire(config().health_interval).await;
        guard(live.pings.recv()).await.unwrap();
        live.updates.send(Ok(Update::Healthy)).unwrap();
        timers.fire(config().health_interval).await;
        guard(live.pings.recv()).await.unwrap();
        live.updates
            .send(Ok(Update::Resources(resources(1))))
            .unwrap();
        guard(supervisor.next_update()).await;
        timers.fire(config().health_timeout).await;
        status(&mut supervisor, MachineId::Local, Status::Reconnecting).await;
        assert!(
            supervisor
                .registry()
                .get(MachineId::Local)
                .unwrap()
                .failure
                .as_ref()
                .unwrap()
                .message
                .contains("health")
        );
        timers.fire(config().retry_initial).await;
        let _attempt = guard(attempts.recv()).await.unwrap();
        supervisor.shutdown().await;
        assert_eq!(live.dropped.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn remote_without_health_still_receives_metadata_and_eof() {
        let machine = machine(1);
        let id = MachineId::Ssh(machine.id);
        let (mut supervisor, mut attempts, _timers) = setup(&[machine]);
        let mut local = None;
        let mut remote = None;
        for _ in 0..2 {
            let attempt = guard(attempts.recv()).await.unwrap();
            if attempt.spec.id() == MachineId::Local {
                local = Some(attempt);
            } else {
                remote = Some(attempt);
            }
        }
        let mut metadata = initial(0);
        metadata.negotiation = Negotiation::Ssh {
            protocol_generation: 1,
            server_version: "test".into(),
            capabilities: Capabilities::default(),
        };
        let (connection, mut live) = connected(metadata);
        assert!(remote.unwrap().answer.send(Ok(connection)).is_ok());
        status(&mut supervisor, id, Status::Ready).await;
        live.updates.send(Ok(Update::Catalog(catalog(1)))).unwrap();
        guard(supervisor.next_update()).await;
        assert!(live.pings.try_recv().is_err());
        drop(live.updates);
        status(&mut supervisor, id, Status::Reconnecting).await;
        assert_eq!(
            supervisor.registry().get(MachineId::Local).unwrap().status,
            Status::Connecting
        );
        supervisor.shutdown().await;
        assert!(local.unwrap().answer.is_closed());
    }

    #[test]
    fn backoff_and_configuration_are_bounded() {
        let settings = config();
        settings.validate().unwrap();
        assert_eq!(settings.retry_delay(u32::MAX), settings.retry_max);
        let tiny = Config {
            retry_initial: Duration::from_nanos(1),
            ..settings.clone()
        };
        assert_eq!(tiny.retry_delay(u32::MAX), tiny.retry_max);
        assert!(
            Config {
                event_capacity: 0,
                ..settings.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            Config {
                retry_initial: Duration::ZERO,
                ..settings.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            Config {
                retry_max: Duration::from_nanos(1),
                ..settings.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            Config {
                health_timeout: Duration::ZERO,
                ..settings
            }
            .validate()
            .is_err()
        );
    }

    #[tokio::test]
    async fn default_constructor_and_drop_cancel_pending_connections() {
        let (sender, mut attempts) = mpsc::unbounded_channel();
        let supervisor =
            Supervisor::new(Arc::new(FakeConnector(sender)), &[], Config::default()).unwrap();
        let mut attempt = guard(attempts.recv()).await.unwrap();
        drop(supervisor);
        guard(attempt.answer.closed()).await;
    }
}
