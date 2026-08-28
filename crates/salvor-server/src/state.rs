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

use crate::client_tools::ClientToolRegistry;
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

/// How often the wake sweeper looks for due timers, unless
/// [`AppState::with_wake_interval`] says otherwise.
///
/// A minute. The unit a durable timer is written in is hours or days, so the
/// resolution that matters is "within a minute of the deadline", and sweeping
/// costs a fold of every run's log (status is not a stored column); a shorter
/// interval would pay that repeatedly to sharpen a number nobody measures.
pub const DEFAULT_WAKE_INTERVAL: Duration = Duration::from_secs(60);

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
    // The client-performed tool DECLARATIONS this server was started with, the
    // declarative sibling of `tool_registry` above. Not an `Option`: an empty
    // set is a complete, honest state (every client-tool intent is a clean
    // `unknown_tool`), because nothing here is ever dispatched, so there is no
    // "the host wired no mechanism" case to tell apart from "the operator
    // declared nothing". Loaded by the operator, never over HTTP; see
    // `crate::client_tools` for why that rule is load-bearing.
    client_tools: Arc<ClientToolRegistry>,
    hooks: Option<(ClockFn, RandomFn)>,
    auth_token: Option<String>,
    poll_interval: Duration,
    // How often the wake sweeper looks for runs whose durable timer has come
    // due. A `Duration` with a real default rather than an `Option`, exactly
    // like `poll_interval` above: a sleeping run nobody re-drives never wakes,
    // so the sweep is part of what serving a store means, not an opt-in. Zero
    // is the off switch (`salvor serve --wake-interval 0`), for an operator who
    // wakes runs from cron with `salvor wake` instead.
    wake_interval: Duration,
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
    // server-driven modes from colliding over one store: the driving
    // client-run endpoints operate only on runs recorded here, so a
    // server-driven run is never reachable through them. It is in-memory
    // because the drive token is a single-writer lease with a process
    // lifetime: a token nobody is holding any more means nothing.
    //
    // Which is why membership here is not the whole answer to "is this run
    // client-driven". This map knows only what this process opened; a run
    // opened before a restart is absent from it while its client is still
    // driving it. The durable half of the answer is the run's own
    // `RunStarted`, which records `driven_by: client` (see
    // `client_runs::log_is_client_driven`). The open endpoint adopts such a
    // run back into this registry, and the surfaces that must not become a
    // second writer (resume, the wake sweeper) consult the log directly.
    client_runs: Mutex<HashMap<RunId, ClientRunLease>>,
    // How long a client-driven run's lease stays "current" without the driver
    // presenting its token again. Past this, the run reports no attached driver
    // on GET /v1/runs (the client-driven half of the liveness evidence): the tab
    // closed, the SDK exited, the driver crashed. It is also how long the run is
    // another driver's to take: a re-open while the lease is current is refused
    // (`409 lease_held`), and a lapsed one is not. Generous by default so a
    // single long model call between drive operations never reads as a false
    // stall, which would also be a window for a second driver to take a run its
    // first driver is still working on; a test shortens it (see
    // `with_client_lease_ttl`) to prove the lapse.
    client_lease_ttl: Duration,
    // Runs the wake sweeper has already warned about being unwakeable here (its
    // agent or graph is not registered in this process). Only the first sighting
    // per run logs at WARN; every later pass while a run's id stays in this set
    // logs the same fields at DEBUG, so an operator who has not fixed the
    // registration gap yet is not paged again every sweep interval, but the
    // fields to find and fix it are still there for anyone who turns on
    // debug-level logging. Cleared when the run wakes or drops out of the due
    // set, so a run that becomes unwakeable again later (a fresh nap, a
    // different recorded agent hash) warns again.
    unwakeable_warned: Mutex<HashSet<RunId>>,
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

/// What asking to drop a client-driven run's lease came to (see
/// [`release_client_run`](AppState::release_client_run)).
///
/// The three outcomes are kept apart because the release endpoint answers each
/// one differently: a release that dropped a lease and a release that found
/// none are both a job done (the run is unheld either way, which is all a
/// caller wanted), while a caller presenting somebody else's token is refused
/// rather than quietly obeyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseRelease {
    /// The caller held the lease, and it is gone.
    Released,
    /// There was no lease on the run to drop: it lapsed, it was released
    /// already, or this process never opened the run at all.
    NoLease,
    /// A lease stands on the run and the caller did not present its token.
    /// Nothing was dropped.
    NotTheHolder,
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
                client_tools: Arc::new(ClientToolRegistry::new()),
                hooks: None,
                auth_token: None,
                poll_interval: Duration::from_millis(50),
                wake_interval: DEFAULT_WAKE_INTERVAL,
                agents: Mutex::new(HashMap::new()),
                graphs: Mutex::new(HashMap::new()),
                active: Mutex::new(HashSet::new()),
                handles: Mutex::new(HashMap::new()),
                client_runs: Mutex::new(HashMap::new()),
                client_lease_ttl: Duration::from_secs(60),
                unwakeable_warned: Mutex::new(HashSet::new()),
            }),
        }
    }

    /// Sets how long a client-driven run's lease stays current without the
    /// driver presenting its token again (default 60s). Past this, the run
    /// reports no attached driver and a re-open may take it from the driver
    /// that has gone quiet. Additive and off-default; a test or a seed
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

    /// Loads the client-performed tool declarations this server answers
    /// client-tool intents against. Additive and empty by default, so no caller
    /// that predates it changes behavior: without a declaration, every
    /// client-tool intent is a clean `unknown_tool` and nothing is written.
    ///
    /// This is the ONLY way declarations enter the process. There is no
    /// endpoint that accepts one, on purpose: a declaration fixes the effect
    /// class, and a client that could declare its own would be choosing whether
    /// its own write is subject to the write-ahead rule. See
    /// [`crate::client_tools`] for the full argument.
    #[must_use]
    pub fn with_client_tools(mut self, decls: Arc<ClientToolRegistry>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("with_client_tools is called before the state is shared")
            .client_tools = decls;
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

    /// Sets how often the wake sweeper looks for runs whose durable timer has
    /// come due (default [`DEFAULT_WAKE_INTERVAL`]). `Duration::ZERO` turns the
    /// sweeper off entirely, for a host that wakes runs some other way.
    ///
    /// Same shape as [`with_poll_interval`](Self::with_poll_interval), and a
    /// test shortens it for the same reason: to make a sweep observable without
    /// waiting on a wall clock.
    #[must_use]
    pub fn with_wake_interval(mut self, interval: Duration) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("with_wake_interval is called before the state is shared")
            .wake_interval = interval;
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

    /// How often the wake sweeper looks for due timers. `Duration::ZERO` means
    /// no sweeper runs on this server.
    #[must_use]
    pub fn wake_interval(&self) -> Duration {
        self.inner.wake_interval
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

    /// The client-performed tool declarations the operator loaded. Empty unless
    /// [`with_client_tools`](Self::with_client_tools) was called, and an empty
    /// set answers every client-tool intent with `unknown_tool`.
    #[must_use]
    pub fn client_tools(&self) -> Arc<ClientToolRegistry> {
        self.inner.client_tools.clone()
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
    /// token. Called by the open endpoint for a new run, and for a re-open the
    /// open endpoint has already decided is allowed: no lease stands, or the
    /// one that does has lapsed, or the run is finished. Minting here is
    /// unconditional, so it must never be reached while another driver's lease
    /// is current; that is the whole point of the check in
    /// [`client_runs::open`](crate::client_runs::open), which reads
    /// [`current_client_lease`](Self::current_client_lease) first.
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

    /// A client-driven run's lease and how long is left before it lapses, when
    /// this process holds one whose driver has proved it is alive within the
    /// lease TTL. `None` covers both "no lease here" (a fresh process, a run
    /// this server never opened) and "the lease lapsed" (the tab closed, the
    /// SDK exited, the driver crashed), because to everything downstream those
    /// are the same fact: nobody is driving this run.
    ///
    /// This is the one place the TTL comparison happens, so the liveness field
    /// on `GET /v1/runs` and the re-open refusal in
    /// [`client_runs::open`](crate::client_runs::open) can never disagree about
    /// whether a driver is still attached. The remaining time comes back with
    /// the lease because the refusal has to tell the second caller when to try
    /// again, and computing it twice would invite the two answers to drift.
    #[must_use]
    pub fn current_client_lease(&self, run_id: RunId) -> Option<(ClientRunLease, Duration)> {
        let now = self.now();
        let leases = self.inner.client_runs.lock().expect("client runs lock");
        let lease = leases.get(&run_id)?;
        let quiet_for = (now - lease.last_seen).unsigned_abs();
        // `checked_sub` is the lapse test: nothing left means the TTL has run
        // out. A remaining of exactly zero is a lapse too, which keeps this
        // strictly-less-than, the comparison this rule has always used.
        let remaining = self.inner.client_lease_ttl.checked_sub(quiet_for)?;
        (!remaining.is_zero()).then(|| (lease.clone(), remaining))
    }

    /// Whether a live driver is currently attached to a client-driven run: this
    /// process holds a lease for it AND the driver presented its token within the
    /// lease TTL. A lapsed lease (the tab closed, the SDK exited) reports `false`:
    /// the client-driven half of the liveness evidence `GET /v1/runs` carries.
    #[must_use]
    pub fn client_run_driver_live(&self, run_id: RunId) -> bool {
        self.current_client_lease(run_id).is_some()
    }

    /// The lease for a client-driven run, if this process opened one under
    /// `run_id`, whether or not its driver has been heard from lately. The
    /// drive-token gate wants exactly this: a driver that went quiet for longer
    /// than the TTL and then presents its token again is still the run's writer,
    /// as long as nobody took the run away from it in the meantime. Ask
    /// [`current_client_lease`](Self::current_client_lease) instead when the
    /// question is whether someone is driving right now.
    #[must_use]
    pub fn client_run(&self, run_id: RunId) -> Option<ClientRunLease> {
        self.inner
            .client_runs
            .lock()
            .expect("client runs lock")
            .get(&run_id)
            .cloned()
    }

    /// How long a client-driven run's lease stays current without the driver
    /// presenting its token again, the TTL every freshness judgment here is
    /// made against. Read by the heartbeat endpoint, which answers with it so a
    /// driver about to be busy for a while knows how often it has to beat.
    #[must_use]
    pub fn client_lease_ttl(&self) -> Duration {
        self.inner.client_lease_ttl
    }

    /// Hands a client-driven run's lease back when `presented` is the token
    /// holding it, so the next open takes the run immediately instead of
    /// waiting out the TTL.
    ///
    /// The token check and the removal happen under one lock, so a release can
    /// only ever drop the lease the caller was actually holding, never one
    /// minted in between by a driver that took the run over.
    ///
    /// Only the lease goes. Nothing about the run itself changes, its recorded
    /// `driven_by: client` included, so a later open adopts it back exactly as
    /// it would after a restart.
    pub fn release_client_run(&self, run_id: RunId, presented: Option<&str>) -> LeaseRelease {
        let mut leases = self.inner.client_runs.lock().expect("client runs lock");
        let Some(lease) = leases.get(&run_id) else {
            return LeaseRelease::NoLease;
        };
        if presented != Some(lease.drive_token.as_str()) {
            return LeaseRelease::NotTheHolder;
        }
        leases.remove(&run_id);
        LeaseRelease::Released
    }

    /// Drops a client-driven run's lease unless `keep` is the token that holds
    /// it, reporting whether a lease went. What a resolve calls: recording a
    /// dangling write by hand says the driver that opened that write never came
    /// back, so the lease it left behind is holding the run for nobody and the
    /// next open should not have to wait out the TTL for it.
    ///
    /// `keep` is how the client-driven resolve keeps its own lease. That caller
    /// presented its current token to get in, which is the driver saying it is
    /// right here, so the lease is not a dead one and taking it away would
    /// strand a driver mid-run. Every other resolve passes `None`, because no
    /// token was presented and nothing says a driver is still attached.
    pub fn clear_client_lease(&self, run_id: RunId, keep: Option<&str>) -> bool {
        let mut leases = self.inner.client_runs.lock().expect("client runs lock");
        let Some(lease) = leases.get(&run_id) else {
            return false;
        };
        if keep == Some(lease.drive_token.as_str()) {
            return false;
        }
        leases.remove(&run_id);
        true
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

    /// Records that the wake sweeper has warned about this run being
    /// unwakeable here. Returns `true` the first time (the caller logs at
    /// WARN) and `false` on every later call for the same run while the
    /// record stands (the caller logs the same fields at DEBUG instead).
    pub fn mark_unwakeable_warned(&self, run_id: RunId) -> bool {
        self.inner
            .unwakeable_warned
            .lock()
            .expect("unwakeable warned lock")
            .insert(run_id)
    }

    /// Whether the sweeper has already warned about this run. Read-only,
    /// unlike [`mark_unwakeable_warned`](Self::mark_unwakeable_warned), which
    /// always records a sighting; a test uses this to check the record
    /// without flipping it.
    #[must_use]
    pub fn unwakeable_warned(&self, run_id: RunId) -> bool {
        self.inner
            .unwakeable_warned
            .lock()
            .expect("unwakeable warned lock")
            .contains(&run_id)
    }

    /// Clears a run's unwakeable-warned record: it woke, so the next time it
    /// naps and cannot be rebuilt here is a fresh first sighting.
    pub fn clear_unwakeable_warned(&self, run_id: RunId) {
        self.inner
            .unwakeable_warned
            .lock()
            .expect("unwakeable warned lock")
            .remove(&run_id);
    }

    /// Drops every unwakeable-warned record for a run not in `still_due`.
    /// Called once per sweep pass before processing, so a run that leaves the
    /// due set some other way than being driven (the only other way its
    /// record could go stale) does not carry a warning into a future nap that
    /// has nothing to do with this one.
    pub fn prune_unwakeable_warned(&self, still_due: &HashSet<RunId>) {
        self.inner
            .unwakeable_warned
            .lock()
            .expect("unwakeable warned lock")
            .retain(|run_id| still_due.contains(run_id));
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
