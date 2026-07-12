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

use salvor_runtime::{Agent, ClockFn, RandomFn, Runtime};
use salvor_store::EventStore;
use salvor_tools::mcp::McpServer;
use tokio::task::JoinHandle;

use salvor_core::RunId;

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
}

/// The shared, cheaply cloned handle every route works through.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn EventStore>,
    factory: AgentFactory,
    hooks: Option<(ClockFn, RandomFn)>,
    auth_token: Option<String>,
    poll_interval: Duration,
    agents: Mutex<HashMap<String, RegisteredAgent>>,
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
                hooks: None,
                auth_token: None,
                poll_interval: Duration::from_millis(50),
                agents: Mutex::new(HashMap::new()),
                active: Mutex::new(HashSet::new()),
                handles: Mutex::new(HashMap::new()),
                client_runs: Mutex::new(HashMap::new()),
            }),
        }
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
                },
            );
        drive_token
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
