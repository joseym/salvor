//! [`AppState`]: the shared handle every request works through, plus the
//! [`AgentFactory`] seam that turns a submitted definition into a live agent.
//!
//! The state owns exactly one thing that matters for durability: an
//! `Arc<dyn EventStore>`. Everything a request needs, it builds fresh from
//! that handle. A [`Runtime`] is cheap (a store handle plus two function
//! pointers), so the server constructs one per request rather than sharing
//! mutable run state; there is no per-run state living in the process that a
//! restart would lose. That is the whole kill-safety story restated: the
//! process holds handles, the store holds truth.
//!
//! # Why agent building is a seam, not baked in
//!
//! Turning a definition into an [`Agent`] means parsing the agent-definition
//! format and spawning its MCP servers. That logic already exists in the CLI
//! (`salvor-cli` owns the TOML schema), and putting a copy here would give the
//! definition format two homes. Instead the server takes an [`AgentFactory`]:
//! a caller-supplied function from a submitted [`AgentDefinition`] to a
//! [`BuiltAgent`]. The `salvor serve` command passes the CLI's own builder, so
//! there is one definition parser in the workspace; tests pass a factory that
//! builds an agent with an in-process tool and a mock model, which is how the
//! control plane is exercised over real HTTP with nothing on the network.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use salvor_graph::Graph;
use salvor_runtime::{Agent, ClockFn, RandomFn, RunCtx, Runtime, RuntimeError};
use salvor_store::EventStore;
use salvor_tools::mcp::McpServer;
use time::OffsetDateTime;
use tokio::task::JoinHandle;

use salvor_core::{EventEnvelope, RunId};

use crate::executor::ModelExecutor;
use crate::tool_registry::ToolRegistry;

/// The format a submitted agent definition is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefFormat {
    /// The agent TOML the CLI reads from a file.
    Toml,
    /// The same definition as a JSON document (what a thin SDK sends).
    Json,
}

/// A submitted agent definition: the raw bytes plus the format they are in.
///
/// The server never interprets the bytes itself; it hands them to the
/// [`AgentFactory`]. Keeping the raw body (rather than a parsed structure)
/// means the definition is rebuilt from exactly what was submitted on every
/// start, resume, and recover, the same way the CLI rebuilds from the TOML
/// file each time.
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    /// The format of `body`.
    pub format: DefFormat,
    /// The raw definition bytes.
    pub body: Vec<u8>,
}

/// A live agent plus the MCP server sessions its tools hold.
///
/// The sessions must outlive the run: each MCP tool keeps a client-peer clone
/// into its server's session, so dropping the sessions stops the tools. The
/// run driver keeps them for the run's life and closes them when it ends.
pub struct BuiltAgent {
    /// The built agent the runtime drives.
    pub agent: Agent,
    /// The MCP sessions to keep alive for the run, then close.
    pub servers: Vec<McpServer>,
}

/// The future an [`AgentFactory`] returns.
pub type BuildFuture = Pin<Box<dyn Future<Output = Result<BuiltAgent, String>> + Send>>;

/// Builds a live agent from a submitted definition.
///
/// The `Err` is a human message; the register and start handlers turn it into
/// a `400`, because a definition that will not build is a client mistake.
pub type AgentFactory = Arc<dyn Fn(AgentDefinition) -> BuildFuture + Send + Sync>;

/// One registered agent definition.
#[derive(Debug, Clone)]
pub struct RegisteredAgent {
    /// The submitted definition, kept for every rebuild.
    pub definition: AgentDefinition,
    /// The agent's content hash (`agent_def_hash`), the id clients reference.
    pub agent_hash: String,
    /// The agent's display name, when the definition declared one
    /// (`Agent::name`, read off the built agent at registration time).
    /// `None` when the definition carried no name: genuinely absent, not a
    /// default to fall back on: [`agents::get`](crate::agents::get) and
    /// [`agents::list`](crate::agents::list) omit the field entirely for
    /// such an agent rather than emit `"name": null`.
    pub name: Option<String>,
}

/// The shared, cheaply cloned handle every route works through.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn EventStore>,
    factory: AgentFactory,
    // The general model-executor seam the server performs a client-driven run's
    // model step through. `None` until a host injects one (the `AgentFactory`
    // pattern): the model-step endpoint then answers with a clear error rather
    // than performing a call. `salvor serve` wires a default from its own
    // client-construction path, so the feature works out of the box.
    model_executor: Option<Arc<dyn ModelExecutor>>,
    // The general tool-registry seam the server performs a client-driven run's
    // tool step through. `None` until a host injects one (the same pattern as
    // `model_executor`): the tool-step endpoint then answers with a clear error
    // rather than dispatching. `salvor serve` wires an EMPTY registry, so any
    // tool-step there is a clean `unknown_tool` until a tool is registered.
    tool_registry: Option<Arc<ToolRegistry>>,
    hooks: Option<(ClockFn, RandomFn)>,
    auth_token: Option<String>,
    poll_interval: Duration,
    agents: Mutex<HashMap<String, RegisteredAgent>>,
    // The graph documents this process has accepted, keyed by their reproducible
    // content hash (`salvor_engine::graph_hash`). In-memory, exactly like the
    // agent registry above and for the same reason: a graph is pure data with a
    // content hash, so re-submitting the identical document is idempotent and a
    // restart re-accepts it under the same hash. Storing it here rather than in
    // the event store keeps the store schema untouched (additive-migration
    // discipline) and mirrors how a registered agent lives only in this map.
    graphs: Mutex<HashMap<String, Graph>>,
    // Which runs a driver task is still working on, and the handles to those
    // tasks. The `active` set is membership only, inserted synchronously
    // before a task is spawned so a concurrent stream can never miss a run
    // that has just started; `handles` is populated after the spawn and used
    // only to abort tasks at shutdown, where a stale finished handle is
    // harmless.
    active: Mutex<HashSet<RunId>>,
    handles: Mutex<HashMap<RunId, JoinHandle<()>>>,
    // The client-driven runs this process has opened, each with its current
    // drive-token lease. This registry is what keeps the client-driven and
    // server-driven modes from colliding over one store: the client-driven
    // endpoints operate only on runs recorded here, so a server-driven run is
    // never reachable through them, and a foreign run id with existing history
    // is refused rather than adopted. It is in-memory because the drive token
    // is a single-writer lease with a process lifetime:
    // re-opening a run mints a fresh lease.
    client_runs: Mutex<HashMap<RunId, ClientRunLease>>,
    // How long a client-driven run's lease stays "current" without the driver
    // presenting its token again. Past this, the run reports no attached driver
    // on GET /v1/runs (the client-driven half of the liveness evidence): the tab
    // closed, the SDK exited, the driver crashed. Generous by default so a single
    // long model call between drive operations never reads as a false stall; a
    // test shortens it (see `with_client_lease_ttl`) to prove the lapse.
    client_lease_ttl: Duration,
}

/// The per-run lease state for a client-driven run.
#[derive(Debug, Clone)]
pub struct ClientRunLease {
    /// The opaque drive token the single writer must present on every append.
    pub drive_token: String,
    /// Whether the opener asked for model request bodies to be recorded. Stored
    /// at open time; it governs the server-performed model step, not yet
    /// implemented, and carries no effect on the generic append this surface
    /// serves.
    pub record_prompts: bool,
    /// When the driver last proved it was alive: stamped at open and refreshed
    /// on every guarded operation (append, model-step, tool-step, resolve), each
    /// of which presents the drive token. That token is the driver's own proof
    /// of life, so its arrival IS the heartbeat: there is no separate mechanism.
    /// Read against the lease TTL to decide whether a driver is still attached to
    /// a client-driven run (see
    /// [`client_run_driver_live`](AppState::client_run_driver_live)).
    pub last_seen: OffsetDateTime,
}

impl AppState {
    /// Builds server state over `store`, using `factory` to turn submitted
    /// definitions into live agents. Auth is off and the clock and random
    /// source are the runtime defaults until set with the `with_*` methods.
    #[must_use]
    pub fn new(store: Arc<dyn EventStore>, factory: AgentFactory) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                factory,
                model_executor: None,
                tool_registry: None,
                hooks: None,
                auth_token: None,
                poll_interval: Duration::from_millis(50),
                agents: Mutex::new(HashMap::new()),
                graphs: Mutex::new(HashMap::new()),
                active: Mutex::new(HashSet::new()),
                handles: Mutex::new(HashMap::new()),
                client_runs: Mutex::new(HashMap::new()),
                client_lease_ttl: Duration::from_secs(60),
            }),
        }
    }

    /// Sets how long a client-driven run's lease stays current without the
    /// driver presenting its token again (default 60s). Past this, the run
    /// reports no attached driver. Additive and off-default; a test or a seed
    /// shortens it to make a driverless client run observable quickly, exactly
    /// as [`with_poll_interval`](Self::with_poll_interval) shortens the stream
    /// poll. `salvor serve` reads it from `SALVOR_CLIENT_LEASE_TTL_SECS`.
    #[must_use]
    pub fn with_client_lease_ttl(mut self, ttl: Duration) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("with_client_lease_ttl is called before the state is shared")
            .client_lease_ttl = ttl;
        self
    }

    /// Requires `Authorization: Bearer <token>` on every request. Without this,
    /// the server trusts its caller (the reverse-proxy posture).
    #[must_use]
    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("with_auth_token is called before the state is shared")
            .auth_token = Some(token.into());
        self
    }

    /// Injects the general model executor the server performs a client-driven
    /// run's model step through. Additive and off by default (the existing
    /// [`new`](Self::new) leaves it unset), so no caller that predates it
    /// changes behavior. `salvor serve` wires a default here; another host
    /// injects its own, exactly as it supplies its own [`AgentFactory`].
    #[must_use]
    pub fn with_model_executor(mut self, executor: Arc<dyn ModelExecutor>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("with_model_executor is called before the state is shared")
            .model_executor = Some(executor);
        self
    }

    /// Injects the general tool registry the server performs a client-driven
    /// run's tool step through. Additive and off by default (the existing
    /// [`new`](Self::new) leaves it unset), so no caller that predates it
    /// changes behavior. `salvor serve` wires an empty registry here; another
    /// host injects one holding its own tools, exactly as it supplies its own
    /// [`AgentFactory`] and [`ModelExecutor`](crate::ModelExecutor).
    #[must_use]
    pub fn with_tool_registry(mut self, registry: Arc<ToolRegistry>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("with_tool_registry is called before the state is shared")
            .tool_registry = Some(registry);
        self
    }

    /// Injects the clock and random source every [`Runtime`] this state builds
    /// uses. Deterministic tests pass fixed functions so full logs compare
    /// equal across a control run and a recovered one.
    #[must_use]
    pub fn with_hooks(mut self, clock: ClockFn, random: RandomFn) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("with_hooks is called before the state is shared")
            .hooks = Some((clock, random));
        self
    }

    /// Sets how often the event stream polls the store for new events (default
    /// 50ms). Tests shorten it so a streamed run completes quickly.
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("with_poll_interval is called before the state is shared")
            .poll_interval = interval;
        self
    }

    /// The event store every request reads from and writes through.
    #[must_use]
    pub fn store(&self) -> Arc<dyn EventStore> {
        self.inner.store.clone()
    }

    /// The expected bearer token, when auth is required.
    #[must_use]
    pub fn auth_token(&self) -> Option<&str> {
        self.inner.auth_token.as_deref()
    }

    /// How often the event stream polls for new events.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        self.inner.poll_interval
    }

    /// The injected model executor, if a host wired one. `None` means the
    /// server cannot perform a model step and the endpoint says so.
    #[must_use]
    pub fn model_executor(&self) -> Option<Arc<dyn ModelExecutor>> {
        self.inner.model_executor.clone()
    }

    /// The injected tool registry, if a host wired one. `None` means the server
    /// cannot perform a tool step and the endpoint says so; a wired-but-empty
    /// registry instead reports each tool as `unknown_tool`.
    #[must_use]
    pub fn tool_registry(&self) -> Option<Arc<ToolRegistry>> {
        self.inner.tool_registry.clone()
    }

    /// Reads the current instant from this state's injected clock, or the real
    /// UTC clock when none was injected. This stamps envelopes the server
    /// records itself (the model-step intent and completion), the same clock
    /// edge a [`Runtime`] would use, so deterministic tests still compare logs.
    #[must_use]
    pub fn now(&self) -> OffsetDateTime {
        match &self.inner.hooks {
            Some((clock, _)) => clock(),
            None => OffsetDateTime::now_utc(),
        }
    }

    /// A fresh runtime over the shared store, with this state's clock and
    /// random source.
    #[must_use]
    pub fn runtime(&self) -> Runtime {
        match &self.inner.hooks {
            Some((clock, random)) => {
                Runtime::with_hooks(self.inner.store.clone(), clock.clone(), random.clone())
            }
            None => Runtime::new(self.inner.store.clone()),
        }
    }

    /// Builds a live agent from a submitted definition, through the factory.
    ///
    /// # Errors
    ///
    /// The factory's human message when the definition will not build.
    pub async fn build_agent(&self, definition: AgentDefinition) -> Result<BuiltAgent, String> {
        (self.inner.factory)(definition).await
    }

    /// Records a registered agent under its content hash, returning that hash.
    pub fn register_agent(&self, registered: RegisteredAgent) -> String {
        let hash = registered.agent_hash.clone();
        self.inner
            .agents
            .lock()
            .expect("agents registry lock")
            .insert(hash.clone(), registered);
        hash
    }

    /// The definition registered under `hash`, if any.
    #[must_use]
    pub fn agent(&self, hash: &str) -> Option<RegisteredAgent> {
        self.inner
            .agents
            .lock()
            .expect("agents registry lock")
            .get(hash)
            .cloned()
    }

    /// Every registered agent's hash, sorted for a stable listing.
    #[must_use]
    pub fn agent_hashes(&self) -> Vec<String> {
        let mut hashes: Vec<String> = self
            .inner
            .agents
            .lock()
            .expect("agents registry lock")
            .keys()
            .cloned()
            .collect();
        hashes.sort();
        hashes
    }

    /// Records a validated graph document under `hash`, returning whether it was
    /// newly stored (`true`) or already present (`false`). Re-storing the
    /// identical document is idempotent: the second call keeps the first and
    /// reports `false`, the graph counterpart of an agent register's `created`.
    pub fn store_graph(&self, hash: String, graph: Graph) -> bool {
        let mut graphs = self.inner.graphs.lock().expect("graphs registry lock");
        if graphs.contains_key(&hash) {
            return false;
        }
        graphs.insert(hash, graph);
        true
    }

    /// The graph document stored under `hash`, if any. `None` is the
    /// `unknown_graph` case.
    #[must_use]
    pub fn graph(&self, hash: &str) -> Option<Graph> {
        self.inner
            .graphs
            .lock()
            .expect("graphs registry lock")
            .get(hash)
            .cloned()
    }

    /// Every stored graph's hash, sorted for a stable listing.
    #[must_use]
    pub fn graph_hashes(&self) -> Vec<String> {
        let mut hashes: Vec<String> = self
            .inner
            .graphs
            .lock()
            .expect("graphs registry lock")
            .keys()
            .cloned()
            .collect();
        hashes.sort();
        hashes
    }

    /// Builds a per-run [`RunCtx`] over `log`, with this state's clock and random
    /// source, so the graph engine can drive a run through the same durability
    /// substrate the built-in loop uses. This is the graph counterpart of
    /// [`runtime`](Self::runtime): the built-in loop reaches the store through a
    /// [`Runtime`]; the graph engine reaches it through a `RunCtx` it drives
    /// directly, and both share the exact clock/random hooks so a deterministic
    /// test's logs still compare equal.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Replay`] when `log` is not a well-formed run history.
    pub fn run_ctx(&self, run_id: RunId, log: Vec<EventEnvelope>) -> Result<RunCtx, RuntimeError> {
        match &self.inner.hooks {
            Some((clock, random)) => RunCtx::with_hooks(
                self.inner.store.clone(),
                run_id,
                log,
                clock.clone(),
                random.clone(),
            ),
            None => RunCtx::new(self.inner.store.clone(), run_id, log),
        }
    }

    /// Marks a run as being driven. Call this synchronously before spawning
    /// the driver task, so a stream opened at the same instant sees the run as
    /// active rather than racing the task's first store write.
    pub fn begin_run(&self, run_id: RunId) {
        self.inner
            .active
            .lock()
            .expect("active runs lock")
            .insert(run_id);
    }

    /// Records the driver task's handle, for aborting at shutdown.
    pub fn set_handle(&self, run_id: RunId, handle: JoinHandle<()>) {
        self.inner
            .handles
            .lock()
            .expect("handles lock")
            .insert(run_id, handle);
    }

    /// Marks a run's drive as ended and drops its handle. The task calls this
    /// as its last act, whether it completed, parked, or errored.
    pub fn end_run(&self, run_id: RunId) {
        self.inner
            .active
            .lock()
            .expect("active runs lock")
            .remove(&run_id);
        self.inner
            .handles
            .lock()
            .expect("handles lock")
            .remove(&run_id);
    }

    /// Whether a run is still being driven by a task in this process.
    #[must_use]
    pub fn is_run_active(&self, run_id: RunId) -> bool {
        self.inner
            .active
            .lock()
            .expect("active runs lock")
            .contains(&run_id)
    }

    /// Records (or re-leases) a client-driven run, returning a fresh drive
    /// token. Called by the open endpoint both for a new run and for a
    /// re-open, so a resuming tab always receives a current lease and any
    /// earlier lease is superseded (the single-writer rule from Q5).
    pub fn lease_client_run(&self, run_id: RunId, record_prompts: bool) -> String {
        let drive_token = format!("dt_{}", uuid::Uuid::new_v4().simple());
        self.inner
            .client_runs
            .lock()
            .expect("client runs lock")
            .insert(
                run_id,
                ClientRunLease {
                    drive_token: drive_token.clone(),
                    record_prompts,
                    last_seen: self.now(),
                },
            );
        drive_token
    }

    /// Refreshes a client-driven run's `last_seen` to now, the driver's proof of
    /// life. Called by the lease gate on every guarded operation (the driver
    /// presented its token, so it is alive); a no-op for a run this process holds
    /// no lease for.
    pub fn touch_client_run(&self, run_id: RunId) {
        let now = self.now();
        if let Some(lease) = self
            .inner
            .client_runs
            .lock()
            .expect("client runs lock")
            .get_mut(&run_id)
        {
            lease.last_seen = now;
        }
    }

    /// Whether a live driver is currently attached to a client-driven run: this
    /// process holds a lease for it AND the driver presented its token within the
    /// lease TTL. A lapsed lease (the tab closed, the SDK exited) reports `false`:
    /// the client-driven half of the liveness evidence `GET /v1/runs` carries.
    #[must_use]
    pub fn client_run_driver_live(&self, run_id: RunId) -> bool {
        let now = self.now();
        let leases = self.inner.client_runs.lock().expect("client runs lock");
        match leases.get(&run_id) {
            Some(lease) => (now - lease.last_seen).unsigned_abs() < self.inner.client_lease_ttl,
            None => false,
        }
    }

    /// The lease for a client-driven run, if this process opened one under
    /// `run_id`.
    #[must_use]
    pub fn client_run(&self, run_id: RunId) -> Option<ClientRunLease> {
        self.inner
            .client_runs
            .lock()
            .expect("client runs lock")
            .get(&run_id)
            .cloned()
    }

    /// Whether `run_id` names a client-driven run this process opened.
    #[must_use]
    pub fn is_client_run(&self, run_id: RunId) -> bool {
        self.inner
            .client_runs
            .lock()
            .expect("client runs lock")
            .contains_key(&run_id)
    }

    /// Aborts every in-flight driver task. Durability is unaffected: each event
    /// was persisted before the task moved on, so an aborted run is recoverable
    /// exactly as after a `kill -9`.
    pub fn abort_all(&self) {
        let mut handles = self.inner.handles.lock().expect("handles lock");
        for (_, handle) in handles.drain() {
            handle.abort();
        }
        self.inner.active.lock().expect("active runs lock").clear();
    }
}
