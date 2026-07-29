//! A line-oriented editor for a graph document, built as an event fold.
//!
//! Someone who wants a graph should not have to hand-write the strict,
//! adjacently tagged JSON [`salvor_graph::document`] defines. This module is
//! the interactive way in: one typed line is one [`Command`], the command list
//! is the state, and the document is a pure function of that list.
//!
//! # Why a fold rather than a mutable document
//!
//! [`Editor`] keeps the applied [`Command`]s and derives the
//! [`salvor_graph::Graph`] from them. Three things fall out of that shape and
//! would each have to be built by hand otherwise:
//!
//! - `undo` is dropping the last command and re-deriving, so no command needs
//!   an inverse. A command that removes a node does not have to remember what
//!   it removed.
//! - a test is a list of lines and an expected document, with no terminal, no
//!   prompt loop, and no file involved.
//! - `history` dumps the list as a script that replays to the identical
//!   document, so a graph built by hand becomes something checked into a repo.
//!
//! # This module performs no IO, and that is load-bearing
//!
//! `salvor-cli-core` compiles for `wasm32-unknown-unknown` so the browser
//! terminal on the landing page runs the REAL editor rather than a second
//! implementation that would drift. Nothing here opens a file, reads the
//! clock, draws randomness, or looks at the environment. Two commands would
//! ordinarily need the host, and both are split so the host half stays outside:
//!
//! - `write` returns the document's JSON as a [`String`] in
//!   [`Outcome::document_json`]. The caller decides where it goes.
//! - `read` takes the document's JSON as a [`String`] in [`Command::Read`].
//!   The caller does the reading.
//!
//! An `agent` node's hash is the third. It cannot be computed here at all:
//! resolving `research.toml` to a `sha256:` hash means parsing TOML and
//! building the agent, which connects to whatever MCP servers the file
//! declares to learn their tool contracts. So the core takes an
//! already-resolved hash, and a line that names a FILE parses to
//! [`Line::Host`] instead of a command. See [`HostRequest`] for that seam.
//!
//! # The grammar
//!
//! [`parse`] turns one line into a [`Line`]. The `help` command prints the
//! whole grammar from the same table the parser is written against, so the
//! editor documents itself:
//!
//! ```text
//! add agent  <ID> --hash <sha256:HASH> | --file <PATH>
//! add tool   <ID> <TOOL> [--input FIELD=SOURCE ...]
//! add gate   <ID> --approval-schema <JSON>
//! add branch <ID> [--on REF] [--hash <sha256:HASH> | --file <PATH>]
//! add map    <ID> --over REF --concurrency N --body <ID>
//! add fold   <ID> --body <ID> --max-iterations N --stop-when EXPR --join J
//! edge <FROM> <TO> [--label NAME]
//! case <BRANCH-ID> <CASE-NAME> --when EXPR | --model
//! rm node <ID> | rm edge <FROM> <TO> | rm case <BRANCH-ID> <CASE-NAME>
//! show [<ID>] | validate | read | write | undo | history | help
//! ```
//!
//! Every node kind's optional `--name`, `--input-schema`, `--output-schema`,
//! `--prompt`, and `--accumulator-schema` follow the model's own field names,
//! so a line names exactly what the document names.
//!
//! # A session, with no terminal involved
//!
//! ```
//! use salvor_cli_core::graph_editor::{Editor, Line, Status, parse};
//! use salvor_cli_core::render::DEFAULT_REPORT_WIDTH;
//!
//! let mut editor = Editor::new();
//! let hash = format!("sha256:{}", "1".repeat(64));
//! for line in [
//!     format!("add agent research --hash {hash}"),
//!     "add gate approve --approval-schema {\"type\":\"object\"}".to_owned(),
//!     "edge research approve".to_owned(),
//! ] {
//!     let Some(Line::Command(command)) = parse(&line).expect("a well-formed line") else {
//!         panic!("this line needs no host");
//!     };
//!     let (next, outcome) = editor.apply(command, DEFAULT_REPORT_WIDTH);
//!     assert_eq!(outcome.status, Status::Ok);
//!     editor = next;
//! }
//!
//! // The document is the fold of the commands, and it validates.
//! let summary = salvor_graph::validate(editor.document()).expect("valid");
//! assert_eq!(summary.entry_nodes, ["research"]);
//!
//! // The session dumps as a script that rebuilds it.
//! assert_eq!(editor.script().lines().count(), 3);
//! ```
//!
//! # A partial document is a legal document
//!
//! Nothing here refuses a command because the result would not validate. An
//! edge may be drawn before its target exists, a branch may carry no cases, an
//! agent hash may be a placeholder someone fixes later. That is not leniency,
//! it is the only way to build a document one line at a time: the first `add`
//! produces a one-node graph with no edges, which passes, but the second line
//! of almost any real session leaves the document momentarily wrong.
//! `validate` is a QUERY the author runs when ready, never a gate on an edit,
//! and it delegates entirely to [`salvor_graph::validate`] so there is exactly
//! one validator in the codebase.

use std::collections::BTreeMap;

use salvor_graph::{
    AgentNode, BranchCase, BranchCondition, BranchNode, Edge, FoldBody, FoldJoin, FoldNode,
    GateNode, Graph, MapBody, MapNode, Node, SCHEMA_VERSION, ToolNode,
};
use serde_json::Value;

use crate::render::{graph_summary, indent, pretty_json, short_hash, wrap};

/// One editor command: the event this module folds into a document.
///
/// Only the first five variants change the document, and only those are
/// recorded in an [`Editor`]'s history. The rest are queries (`Show`,
/// `Validate`, `Write`, `History`, `Help`) or edit the history itself
/// (`Undo`), so a dumped script is exactly the commands that shaped the
/// document and nothing else.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Append a node. Boxed because a [`Node`] carries several optional JSON
    /// schemas and would otherwise make every other variant as large.
    Add(Box<Node>),
    /// Append an edge. The endpoints need not exist yet.
    Edge(Edge),
    /// Append a case to an existing `branch` node.
    Case {
        /// The branch node's id.
        node: String,
        /// The case to append.
        case: BranchCase,
    },
    /// Remove a node, an edge, or a branch case.
    Remove(Target),
    /// Replace the whole document with the one this JSON parses to.
    ///
    /// The JSON is carried INLINE rather than as a path, which is what keeps a
    /// dumped history replayable with no file present and lets `undo` step
    /// back across a read like any other event.
    Read {
        /// The document's JSON text.
        json: String,
    },
    /// Show the document as an outline, or one node in full.
    Show {
        /// The node to show in full, or `None` for the whole outline.
        node: Option<String>,
    },
    /// Run [`salvor_graph::validate`] and report every problem it found.
    Validate,
    /// Serialize the document, returning its JSON in
    /// [`Outcome::document_json`].
    Write,
    /// Drop the last recorded command and re-derive the document.
    Undo,
    /// Dump the recorded history as a replayable script.
    History,
    /// Print the grammar, all of it or one command's.
    Help {
        /// The command to explain, or `None` for the overview.
        topic: Option<String>,
    },
}

/// What a [`Command::Remove`] removes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A node, and every edge that names it. An edge is pure topology and
    /// cannot mean anything without both endpoints, so removing a node takes
    /// its incident edges with it. A `map` or `fold` body that names the node
    /// is left alone, because silently editing ANOTHER node's payload is worse
    /// than the dangling reference `validate` will name.
    Node {
        /// The node id to remove.
        id: String,
    },
    /// Every edge between two nodes. With no `label`, every edge from `from`
    /// to `to` whatever its label; with one, only the edges carrying it.
    Edge {
        /// The edge's source.
        from: String,
        /// The edge's destination.
        to: String,
        /// The label to narrow to, or `None` for every label.
        label: Option<String>,
    },
    /// One case of a `branch` node.
    Case {
        /// The branch node's id.
        node: String,
        /// The case name to remove.
        case: String,
    },
}

/// What one line of input turned out to be.
#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    /// A command the editor applies as it stands.
    Command(Command),
    /// The line named something only the host can turn into data. This crate
    /// cannot touch it, so the caller resolves it and applies the command it
    /// gets back. See [`HostRequest`].
    Host(HostRequest),
}

/// A line that needs the host before it can become a [`Command`].
///
/// This is the whole seam between the pure editor and a caller that owns a
/// filesystem. Every variant names a path EXACTLY as it was typed and does
/// nothing with it; resolving it is the caller's job, and a caller with no
/// filesystem (the browser terminal) refuses the line instead.
///
/// A dumped history never contains one of these forms: `to_line` always emits
/// the already-resolved shape (`--hash`, `--json`), so a script replays with
/// no host access at all.
#[derive(Clone, Debug, PartialEq)]
pub enum HostRequest {
    /// `add agent <ID> --file <PATH>`, or the same on `add branch`.
    ///
    /// The caller resolves `path` to a `sha256:<64 hex>` agent definition hash
    /// (the `salvor agent hash` path: parse the TOML, build the agent,
    /// connecting to its MCP servers for their tool schemas, and hash the
    /// BUILT definition) and then calls [`HashDraft::with_hash`] to get the
    /// command to apply.
    AgentFile {
        /// The path the line named, verbatim.
        path: String,
        /// The command, complete but for the hash.
        draft: HashDraft,
    },
    /// `read <PATH>`. The caller reads `path` and applies
    /// [`Command::Read`] with its contents.
    ReadFile {
        /// The path the line named, verbatim.
        path: String,
    },
    /// `write <PATH>`. The caller applies [`Command::Write`] and writes the
    /// returned [`Outcome::document_json`] to `path`.
    WriteFile {
        /// The path the line named, verbatim.
        path: String,
    },
}

/// An `add` command that is complete except for an agent definition's hash.
///
/// Its fields are private and the only way out is [`with_hash`](Self::with_hash),
/// so the placeholder it holds in the meantime cannot reach a document.
#[derive(Clone, Debug, PartialEq)]
pub struct HashDraft {
    /// The node, with its hash field left empty.
    node: Node,
}

impl HashDraft {
    /// The id of the node being added, for a message the caller prints while
    /// it resolves the file.
    #[must_use]
    pub fn node_id(&self) -> &str {
        self.node.id()
    }

    /// The kind of node being added (`"agent"` or `"branch"`).
    #[must_use]
    pub fn node_kind(&self) -> &'static str {
        self.node.kind_name()
    }

    /// Completes the draft with a resolved `sha256:<64 hex>` hash, yielding the
    /// command to apply. The hash's FORM is checked by
    /// [`salvor_graph::validate`], not here, exactly as it is for a hash typed
    /// straight into `--hash`.
    #[must_use]
    pub fn with_hash(mut self, agent_hash: impl Into<String>) -> Command {
        match &mut self.node {
            Node::Agent(node) => node.agent_hash = agent_hash.into(),
            Node::Branch(node) => node.agent_hash = Some(agent_hash.into()),
            // No other kind carries an agent hash, and the parser builds no
            // other kind into a draft, so there is nothing to fill in.
            Node::Tool(_) | Node::Gate(_) | Node::Map(_) | Node::Fold(_) => {}
        }
        Command::Add(Box::new(self.node))
    }
}

/// How an applied command turned out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The command did what it said.
    Ok,
    /// The command was refused and the document is unchanged: an unknown node
    /// id, a duplicate id, nothing left to undo.
    Refused,
    /// The command ran, and reported a problem with the DOCUMENT rather than
    /// with itself. Only `validate` on a document that does not pass produces
    /// this: asking a broken document to validate is not a failed command.
    Invalid,
}

/// What one applied command produced.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    /// The text to display, ending in a newline.
    pub text: String,
    /// How the command turned out.
    pub status: Status,
    /// The serialized document, present only for [`Command::Write`]. This
    /// crate cannot write a file, so the bytes come back here and the caller
    /// decides where they go.
    pub document_json: Option<String>,
}

impl Outcome {
    /// An outcome that only has text to show.
    fn shown(text: String) -> Self {
        Self {
            text,
            status: Status::Ok,
            document_json: None,
        }
    }

    /// A refusal. The document is unchanged.
    fn refused(text: String) -> Self {
        Self {
            text: format!("{text}\n"),
            status: Status::Refused,
            document_json: None,
        }
    }

    /// Folds a fallible query into an outcome.
    fn from_result(result: Result<String, String>) -> Self {
        match result {
            Ok(text) => Self::shown(text),
            Err(why) => Self::refused(why),
        }
    }
}

/// The editor's whole state: the commands applied so far, and the document
/// they derive to.
///
/// The history is the state of record. `document` is a cache that is ALWAYS
/// exactly [`derive_document`] over `history`, re-derived from empty on every
/// change rather than patched in place, which is what makes `undo` a
/// truncation and keeps the two from drifting. A document is a handful of
/// nodes, so re-deriving costs nothing worth optimizing, and a test asserts
/// the invariant directly.
#[derive(Clone, Debug, PartialEq)]
pub struct Editor {
    history: Vec<Command>,
    document: Graph,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    /// An editor holding an empty document: no commands, no nodes, no edges,
    /// stamped with the current [`SCHEMA_VERSION`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            history: Vec::new(),
            document: empty_document(),
        }
    }

    /// An editor whose history is `history`, with the document derived from it.
    ///
    /// This is how a dumped script is replayed, and it is the statement that
    /// the state is a pure function of the command list: an editor built this
    /// way is indistinguishable from one that had the same commands typed into
    /// it one at a time.
    #[must_use]
    pub fn from_history(history: Vec<Command>) -> Self {
        let document = derive_document(&history);
        Self { history, document }
    }

    /// The document as it stands. Not necessarily valid; see the module docs'
    /// "A partial document is a legal document".
    #[must_use]
    pub fn document(&self) -> &Graph {
        &self.document
    }

    /// The commands recorded so far, oldest first. Only document-changing
    /// commands appear.
    #[must_use]
    pub fn history(&self) -> &[Command] {
        &self.history
    }

    /// The recorded history as a replayable script, one command per line.
    ///
    /// Feeding these lines back through [`parse`] and [`Editor::apply`] yields
    /// an identical document, and every line is in the already-resolved form,
    /// so the script needs no filesystem.
    #[must_use]
    pub fn script(&self) -> String {
        let mut out = String::new();
        for command in &self.history {
            out.push_str(&command.to_line());
            out.push('\n');
        }
        out
    }

    /// Applies one command, returning the next editor and the output to show.
    ///
    /// `width` is the column count prose wraps to; a caller that knows its own
    /// pane passes that, and [`crate::render::DEFAULT_REPORT_WIDTH`] is the
    /// terminal default. This is the whole surface the wasm boundary needs: a
    /// pure `(state, command) -> (state, output)`.
    #[must_use]
    pub fn apply(mut self, command: Command, width: usize) -> (Self, Outcome) {
        // Queries and history edits first: none of them is recorded, which is
        // what makes a dumped script exactly the commands that shaped the
        // document.
        match command {
            Command::Show { ref node } => {
                let result = self.show(node.as_deref(), width);
                return (self, Outcome::from_result(result));
            }
            Command::Validate => {
                let (text, status) = self.report(width);
                return (
                    self,
                    Outcome {
                        text,
                        status,
                        document_json: None,
                    },
                );
            }
            Command::Write => {
                let json = self.document_json();
                let text = format!(
                    "serialized {} node(s) and {} edge(s), {} bytes of JSON\n",
                    self.document.nodes.len(),
                    self.document.edges.len(),
                    json.len(),
                );
                return (
                    self,
                    Outcome {
                        text,
                        status: Status::Ok,
                        document_json: Some(json),
                    },
                );
            }
            Command::History => {
                let text = if self.history.is_empty() {
                    wrap(
                        "No commands recorded yet, so there is no script to dump.",
                        width,
                        "",
                        "",
                    ) + "\n"
                } else {
                    self.script()
                };
                return (self, Outcome::shown(text));
            }
            Command::Help { ref topic } => {
                let result = help(topic.as_deref(), width);
                return (self, Outcome::from_result(result));
            }
            Command::Undo => {
                let Some(dropped) = self.history.pop() else {
                    return (self, Outcome::refused("nothing to undo".to_owned()));
                };
                self.document = derive_document(&self.history);
                let text = format!(
                    "undid `{}`\n{} command(s) still recorded\n",
                    dropped.to_line(),
                    self.history.len(),
                );
                return (self, Outcome::shown(text));
            }
            Command::Add(_)
            | Command::Edge(_)
            | Command::Case { .. }
            | Command::Remove(_)
            | Command::Read { .. } => {}
        }

        // A document-changing command. `check` reads the CURRENT document, so
        // it can both refuse an inapplicable command and count what a removal
        // is about to take with it.
        match self.check(&command, width) {
            Err(why) => (self, Outcome::refused(why)),
            Ok(text) => {
                self.history.push(command);
                self.document = derive_document(&self.history);
                (self, Outcome::shown(text))
            }
        }
    }

    /// The document's pretty JSON, newline-terminated so it is a well-formed
    /// file the moment a caller writes it.
    fn document_json(&self) -> String {
        let mut json = serde_json::to_string_pretty(&self.document)
            .expect("a graph document is always serializable");
        json.push('\n');
        json
    }

    /// Refuses an inapplicable document-changing command, or returns the
    /// message its application should print.
    fn check(&self, command: &Command, width: usize) -> Result<String, String> {
        match command {
            Command::Add(node) => {
                if self.node(node.id()).is_some() {
                    return Err(format!(
                        "a node `{}` is already in the document; remove it first, or pick \
                         another id",
                        node.id()
                    ));
                }
                Ok(format!("added {} node `{}`\n", node.kind_name(), node.id()))
            }
            Command::Edge(edge) => {
                let mut out = format!("added edge `{}` -> `{}`", edge.from, edge.to);
                if let Some(label) = &edge.label {
                    out.push_str(&format!(" labeled `{label}`"));
                }
                out.push('\n');
                // An edge drawn ahead of its endpoint is deliberately allowed;
                // see the module docs. Saying so is the difference between a
                // legal intermediate state and a silent mistake.
                let missing: Vec<&str> = [edge.from.as_str(), edge.to.as_str()]
                    .into_iter()
                    .filter(|id| self.node(id).is_none())
                    .collect();
                if !missing.is_empty() {
                    let ids = missing
                        .iter()
                        .map(|id| format!("`{id}`"))
                        .collect::<Vec<_>>()
                        .join(" and ");
                    out.push_str(&wrap(
                        &format!(
                            "no node {ids} in the document yet, so validate reports the \
                             dangling endpoint until one is added"
                        ),
                        width,
                        "note: ",
                        "      ",
                    ));
                    out.push('\n');
                }
                Ok(out)
            }
            Command::Case { node, case } => {
                let branch = self.branch(node)?;
                if branch
                    .cases
                    .iter()
                    .any(|existing| existing.name == case.name)
                {
                    return Err(format!(
                        "branch `{node}` already has a case `{}`",
                        case.name
                    ));
                }
                Ok(format!("added case `{}` to branch `{node}`\n", case.name))
            }
            Command::Remove(target) => self.check_remove(target),
            Command::Read { json } => {
                let graph = parse_document(json)?;
                Ok(wrap(
                    &format!(
                        "read a document of {} node(s) and {} edge(s), replacing the {}",
                        graph.nodes.len(),
                        graph.edges.len(),
                        describe_size(&self.document),
                    ),
                    width,
                    "",
                    "",
                ) + "\n")
            }
            // Handled before `check` is ever reached.
            Command::Show { .. }
            | Command::Validate
            | Command::Write
            | Command::Undo
            | Command::History
            | Command::Help { .. } => Ok(String::new()),
        }
    }

    /// The removal half of [`Self::check`], kept separate because each target
    /// counts a different thing.
    fn check_remove(&self, target: &Target) -> Result<String, String> {
        match target {
            Target::Node { id } => {
                if self.node(id).is_none() {
                    return Err(format!("no node `{id}` in the document"));
                }
                let incident = self
                    .document
                    .edges
                    .iter()
                    .filter(|edge| edge.from == *id || edge.to == *id)
                    .count();
                if incident == 0 {
                    Ok(format!("removed node `{id}`\n"))
                } else {
                    Ok(format!(
                        "removed node `{id}` and its {incident} incident edge(s)\n"
                    ))
                }
            }
            Target::Edge { from, to, label } => {
                let matched = self
                    .document
                    .edges
                    .iter()
                    .filter(|edge| edge_matches(edge, from, to, label.as_deref()))
                    .count();
                if matched == 0 {
                    return Err(match label {
                        Some(label) => {
                            format!("no edge `{from}` -> `{to}` labeled `{label}` in the document")
                        }
                        None => format!("no edge `{from}` -> `{to}` in the document"),
                    });
                }
                Ok(format!("removed {matched} edge(s) `{from}` -> `{to}`\n"))
            }
            Target::Case { node, case } => {
                let branch = self.branch(node)?;
                if !branch.cases.iter().any(|existing| existing.name == *case) {
                    return Err(format!("branch `{node}` has no case `{case}`"));
                }
                Ok(format!("removed case `{case}` from branch `{node}`\n"))
            }
        }
    }

    /// The node with this id, if the document has one.
    fn node(&self, id: &str) -> Option<&Node> {
        self.document.nodes.iter().find(|node| node.id() == id)
    }

    /// The `branch` node with this id, refusing with a precise message when
    /// there is no such node or it is some other kind. `case` mutates a node
    /// that must already exist, so this is a genuine refusal rather than an
    /// incomplete document.
    fn branch(&self, id: &str) -> Result<&BranchNode, String> {
        match self.node(id) {
            Some(Node::Branch(branch)) => Ok(branch),
            Some(other) => Err(format!(
                "node `{id}` is a {} node, and only a branch node has cases",
                other.kind_name()
            )),
            None => Err(format!("no node `{id}` in the document")),
        }
    }
}

/// An empty document stamped with the current schema version, which is what
/// [`salvor_graph::GraphBuilder::build`] stamps too.
fn empty_document() -> Graph {
    Graph {
        schema_version: SCHEMA_VERSION,
        nodes: Vec::new(),
        edges: Vec::new(),
    }
}

/// Whether an edge matches a removal target. With no label, every edge between
/// the two nodes matches whatever it is labeled; with one, only the edges
/// carrying it.
fn edge_matches(edge: &Edge, from: &str, to: &str, label: Option<&str>) -> bool {
    edge.from == from
        && edge.to == to
        && match label {
            Some(label) => edge.label.as_deref() == Some(label),
            None => true,
        }
}

/// Parses a document, turning serde's message into a refusal. Strict parsing
/// means the message already names a stray or missing field, so there is
/// nothing to add to it.
fn parse_document(json: &str) -> Result<Graph, String> {
    serde_json::from_str::<Graph>(json)
        .map_err(|error| format!("that is not a graph document: {error}"))
}

/// "empty document" or "document of N node(s) and M edge(s)", for a message
/// that has to say what a `read` replaced.
fn describe_size(graph: &Graph) -> String {
    if graph.nodes.is_empty() && graph.edges.is_empty() {
        "empty document".to_owned()
    } else {
        format!(
            "document of {} node(s) and {} edge(s)",
            graph.nodes.len(),
            graph.edges.len()
        )
    }
}

/// Derives the document from a command list: the fold at the heart of this
/// module.
///
/// Every non-changing command is skipped, so replaying a list that contains
/// queries yields the same document as replaying one that does not. A `Read`
/// whose JSON does not parse is skipped rather than panicking, which cannot
/// happen through [`Editor::apply`] (it refuses an unparseable read before
/// recording it) but keeps a hand-built history total.
fn derive_document(history: &[Command]) -> Graph {
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    // Preserved rather than re-stamped: a version a `read` brought in is the
    // author's, and silently rewriting it would make `write` change a field
    // nobody edited. Whether this build understands that version is
    // `salvor_graph::validate`'s call, not this function's.
    let mut schema_version = SCHEMA_VERSION;

    for command in history {
        match command {
            Command::Add(node) => nodes.push((**node).clone()),
            Command::Edge(edge) => edges.push(edge.clone()),
            Command::Case { node, case } => {
                if let Some(Node::Branch(branch)) =
                    nodes.iter_mut().find(|existing| existing.id() == node)
                {
                    branch.cases.push(case.clone());
                }
            }
            Command::Remove(Target::Node { id }) => {
                nodes.retain(|node| node.id() != id);
                edges.retain(|edge| edge.from != *id && edge.to != *id);
            }
            Command::Remove(Target::Edge { from, to, label }) => {
                edges.retain(|edge| !edge_matches(edge, from, to, label.as_deref()));
            }
            Command::Remove(Target::Case { node, case }) => {
                if let Some(Node::Branch(branch)) =
                    nodes.iter_mut().find(|existing| existing.id() == node)
                {
                    branch.cases.retain(|existing| existing.name != *case);
                }
            }
            Command::Read { json } => {
                if let Ok(graph) = serde_json::from_str::<Graph>(json) {
                    schema_version = graph.schema_version;
                    nodes = graph.nodes;
                    edges = graph.edges;
                }
            }
            Command::Show { .. }
            | Command::Validate
            | Command::Write
            | Command::Undo
            | Command::History
            | Command::Help { .. } => {}
        }
    }

    Graph {
        schema_version,
        nodes,
        edges,
    }
}

// --- what the reader sees ---------------------------------------------------

impl Editor {
    /// The `show` output: the whole document as an outline, or one node in
    /// full.
    ///
    /// The outline deliberately is NOT the JSON. `write` is what produces JSON;
    /// `show` is what an author reads after every edit, in a pane that may be
    /// narrow, so it gives one wrapped line per node and one per edge. A
    /// 71-character agent hash is shortened by [`short_hash`] and a JSON schema
    /// is reduced to its declared `type`, because either printed in full would
    /// swamp the pane and neither is what the author is checking. `show <ID>`
    /// is where the full hash and the whole pretty-printed schema live.
    ///
    /// Only the six-character kind word is column-aligned. Nothing else is, so
    /// a long node id cannot shear the layout, and every detail span is
    /// wrapped with a hanging indent that lines continuations up under the id.
    fn show(&self, node: Option<&str>, width: usize) -> Result<String, String> {
        match node {
            Some(id) => match self.node(id) {
                Some(node) => Ok(node_detail(node)),
                None => Err(format!("no node `{id}` in the document")),
            },
            None => Ok(self.outline(width)),
        }
    }

    /// The whole-document outline half of [`Self::show`].
    fn outline(&self, width: usize) -> String {
        if self.document.nodes.is_empty() && self.document.edges.is_empty() {
            let mut out = wrap(
                &format!(
                    "graph: empty, schema_version {}",
                    self.document.schema_version
                ),
                width,
                "",
                "",
            );
            out.push_str("\n\n");
            out.push_str(&wrap(
                "Nothing added yet. `help` lists every command, and `add agent <ID> --hash \
                 <sha256:HASH>` starts a document.",
                width,
                "",
                "",
            ));
            out.push('\n');
            return out;
        }

        let mut out = wrap(
            &format!(
                "graph: {} node(s), {} edge(s), schema_version {}",
                self.document.nodes.len(),
                self.document.edges.len(),
                self.document.schema_version,
            ),
            width,
            "",
            "",
        );
        out.push('\n');

        if !self.document.nodes.is_empty() {
            out.push_str("\nnodes\n");
            for node in &self.document.nodes {
                // The kind word is at most six characters ("branch"), so this
                // one aligned column is safe at any width.
                let first = format!("  {:<6}  {}  ", node.kind_name(), node.id());
                let digest = node_digest(node);
                if digest.is_empty() {
                    out.push_str(first.trim_end());
                } else {
                    out.push_str(&wrap(&digest, width, &first, "          "));
                }
                out.push('\n');
            }
        }

        if !self.document.edges.is_empty() {
            out.push_str("\nedges\n");
            for edge in &self.document.edges {
                let mut text = format!("{} -> {}", edge.from, edge.to);
                if let Some(label) = &edge.label {
                    text.push_str(&format!(" [{label}]"));
                }
                out.push_str(&wrap(&text, width, "  ", "    "));
                out.push('\n');
            }
        }

        out
    }

    /// The `validate` output, and the status it earns.
    ///
    /// [`salvor_graph::validate`] is the only validator: it runs every
    /// independent check and returns EVERY failure, each already naming the
    /// node or edge at fault, so this function only numbers and wraps them.
    /// Success reuses [`graph_summary`], the same text `salvor graph validate`
    /// prints, so the two surfaces cannot describe one document two ways.
    fn report(&self, width: usize) -> (String, Status) {
        match salvor_graph::validate(&self.document) {
            Ok(summary) => (graph_summary(&summary), Status::Ok),
            Err(errors) => {
                let mut out = format!("graph invalid: {} problem(s)\n", errors.len());
                for (index, error) in errors.iter().enumerate() {
                    let first = format!("  {}. ", index + 1);
                    let rest = " ".repeat(first.len());
                    out.push_str(&wrap(&error.to_string(), width, &first, &rest));
                    out.push('\n');
                }
                (out, Status::Invalid)
            }
        }
    }
}

/// The one-line digest of a node for the outline: its distinguishing fields,
/// shortened enough to read in a narrow pane. Empty when the node has nothing
/// to say beyond its kind and id.
fn node_digest(node: &Node) -> String {
    let mut facts: Vec<String> = Vec::new();
    if let Some(name) = node.name() {
        facts.push(quote(name));
    }
    match node {
        Node::Agent(agent) => facts.push(format!("hash {}", short_hash(&agent.agent_hash))),
        Node::Tool(tool) => {
            facts.push(format!("tool {}", tool.tool));
            for (field, source) in &tool.input {
                facts.push(format!("input {field}={source}"));
            }
        }
        Node::Gate(gate) => {
            if let Some(prompt) = &gate.prompt {
                facts.push(format!("prompt {}", quote(prompt)));
            }
            facts.push(format!(
                "approval_schema {}",
                schema_word(&gate.approval_schema)
            ));
        }
        Node::Branch(branch) => {
            if let Some(on) = &branch.on {
                facts.push(format!("on {on}"));
            }
            if let Some(hash) = &branch.agent_hash {
                facts.push(format!("hash {}", short_hash(hash)));
            }
            if branch.cases.is_empty() {
                facts.push("no cases yet".to_owned());
            }
            for case in &branch.cases {
                facts.push(case_word(case));
            }
        }
        Node::Map(map) => {
            facts.push(format!("over {}", map.over));
            facts.push(format!("concurrency {}", map.concurrency));
            facts.push(map_body_word(&map.body));
        }
        Node::Fold(fold) => {
            facts.push(fold_body_word(&fold.body));
            facts.push(format!("max_iterations {}", fold.max_iterations));
            facts.push(format!("stop_when {}", quote(&fold.stop_when)));
            facts.push(join_word(&fold.join));
            if let Some(schema) = &fold.accumulator_schema {
                facts.push(format!("accumulator_schema {}", schema_word(schema)));
            }
        }
    }
    if let Some(schema) = node.input_schema() {
        facts.push(format!("input_schema {}", schema_word(schema)));
    }
    if let Some(schema) = node.output_schema() {
        facts.push(format!("output_schema {}", schema_word(schema)));
    }
    facts.join(", ")
}

/// One node in full, for `show <ID>`: an aligned label column and the schemas
/// pretty-printed underneath.
///
/// Neither the aligned block nor the JSON is wrapped, following the same rule
/// [`crate::render`] keeps: a reader checks these against a file, so reflowing
/// them by pane width would break the alignment they exist for.
fn node_detail(node: &Node) -> String {
    let mut out = format!("{} node `{}`\n", node.kind_name(), node.id());
    let mut field = |label: &str, value: &str| {
        out.push_str(&format!("  {label:<20} {value}\n"));
    };
    if let Some(name) = node.name() {
        field("name:", name);
    }
    match node {
        Node::Agent(agent) => field("agent_hash:", &agent.agent_hash),
        Node::Tool(tool) => {
            field("tool:", &tool.tool);
            for (key, source) in &tool.input {
                field(&format!("input {key}:"), source);
            }
        }
        Node::Gate(gate) => {
            if let Some(prompt) = &gate.prompt {
                field("prompt:", prompt);
            }
        }
        Node::Branch(branch) => {
            if let Some(on) = &branch.on {
                field("on:", on);
            }
            if let Some(hash) = &branch.agent_hash {
                field("agent_hash:", hash);
            }
            if branch.cases.is_empty() {
                field("cases:", "(none yet)");
            }
            for case in &branch.cases {
                field("case:", &case_word(case));
            }
        }
        Node::Map(map) => {
            field("over:", &map.over);
            field("concurrency:", &map.concurrency.to_string());
            field("body:", &map_body_word(&map.body));
        }
        Node::Fold(fold) => {
            field("body:", &fold_body_word(&fold.body));
            field("max_iterations:", &fold.max_iterations.to_string());
            field("stop_when:", &fold.stop_when);
            field("join:", &join_word(&fold.join));
        }
    }

    let mut schema = |label: &str, value: &Value| {
        out.push_str(&format!("  {label}\n{}\n", indent(&pretty_json(value), 4)));
    };
    if let Node::Gate(gate) = node {
        schema("approval_schema:", &gate.approval_schema);
    }
    if let Some(value) = node.input_schema() {
        schema("input_schema:", value);
    }
    if let Some(value) = node.output_schema() {
        schema("output_schema:", value);
    }
    if let Node::Fold(fold) = node
        && let Some(value) = &fold.accumulator_schema
    {
        schema("accumulator_schema:", value);
    }
    out
}

/// A schema reduced to one word for the outline: its declared `type`, a union
/// of types, `$ref`, or a bare "schema" for anything else. The whole schema is
/// a `show <ID>` away, and printing it in the outline would bury the topology
/// the outline exists to show.
fn schema_word(schema: &Value) -> String {
    match schema.get("type") {
        Some(Value::String(word)) => word.clone(),
        Some(Value::Array(words)) => words
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("|"),
        _ if schema.get("$ref").is_some() => "$ref".to_owned(),
        _ => "schema".to_owned(),
    }
}

/// One branch case, as the outline and the detail block both name it.
fn case_word(case: &BranchCase) -> String {
    match &case.when {
        BranchCondition::Expression(expression) => {
            format!("case {} when {}", case.name, quote(expression))
        }
        BranchCondition::ModelDecision => format!("case {} by model", case.name),
    }
}

/// A map body, named for a reader.
fn map_body_word(body: &MapBody) -> String {
    match body {
        MapBody::Node(id) => format!("body node {id}"),
        MapBody::Subgraph(graph) => format!("body subgraph of {} node(s)", graph.nodes.len()),
    }
}

/// A fold body, named for a reader.
fn fold_body_word(body: &FoldBody) -> String {
    match body {
        FoldBody::Node(id) => format!("body node {id}"),
        FoldBody::Subgraph(graph) => format!("body subgraph of {} node(s)", graph.nodes.len()),
    }
}

/// A fold join rule, named for a reader. Deliberately the same words the
/// `--join` option takes, so what `show` prints is what a line types.
fn join_word(join: &FoldJoin) -> String {
    match join {
        FoldJoin::BestBy(reference) => format!("join best-by:{reference}"),
        FoldJoin::Last => "join last".to_owned(),
        FoldJoin::All => "join all".to_owned(),
    }
}

// --- dumping a command back to a line ---------------------------------------

impl Command {
    /// The line that reproduces this command, exactly as [`parse`] accepts it.
    ///
    /// Always the already-resolved form: `--hash`, never `--file`, and `read
    /// --json <JSON>`, never `read <PATH>`. That is what makes a dumped
    /// [`Editor::script`] replay with no filesystem, in a browser as readily as
    /// on a machine that has the original files.
    ///
    /// This is also the reason [`Command::Add`] carries a [`Node`] rather than
    /// one of [`salvor_graph`]'s builder specs: dumping a command means reading
    /// its values back out as text, and a spec's fields are private.
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            Command::Add(node) => add_line(node),
            Command::Edge(edge) => {
                let mut out = format!("edge {} {}", word(&edge.from), word(&edge.to));
                if let Some(label) = &edge.label {
                    out.push_str(&format!(" --label {}", word(label)));
                }
                out
            }
            Command::Case { node, case } => {
                let mut out = format!("case {} {}", word(node), word(&case.name));
                match &case.when {
                    BranchCondition::Expression(expression) => {
                        out.push_str(&format!(" --when {}", word(expression)));
                    }
                    BranchCondition::ModelDecision => out.push_str(" --model"),
                }
                out
            }
            Command::Remove(Target::Node { id }) => format!("rm node {}", word(id)),
            Command::Remove(Target::Edge { from, to, label }) => {
                let mut out = format!("rm edge {} {}", word(from), word(to));
                if let Some(label) = label {
                    out.push_str(&format!(" --label {}", word(label)));
                }
                out
            }
            Command::Remove(Target::Case { node, case }) => {
                format!("rm case {} {}", word(node), word(case))
            }
            // Re-serialized compactly so the whole document stays on ONE line,
            // which is what keeps a dumped script line-oriented. A recorded
            // read always parsed once already; the raw text is the fallback for
            // a hand-built history that never went through `apply`.
            Command::Read { json } => match serde_json::from_str::<Graph>(json) {
                Ok(graph) => format!("read --json {}", compact_graph(&graph)),
                Err(_) => format!("read --json {json}"),
            },
            Command::Show { node } => match node {
                Some(id) => format!("show {}", word(id)),
                None => "show".to_owned(),
            },
            Command::Validate => "validate".to_owned(),
            Command::Write => "write".to_owned(),
            Command::Undo => "undo".to_owned(),
            Command::History => "history".to_owned(),
            Command::Help { topic } => match topic {
                Some(topic) => format!("help {}", word(topic)),
                None => "help".to_owned(),
            },
        }
    }
}

/// The `add` half of [`Command::to_line`], one arm per node kind.
fn add_line(node: &Node) -> String {
    let mut out = format!("add {} {}", node.kind_name(), word(node.id()));
    match node {
        Node::Agent(agent) => out.push_str(&format!(" --hash {}", word(&agent.agent_hash))),
        Node::Tool(tool) => {
            out.push_str(&format!(" {}", word(&tool.tool)));
            for (field, source) in &tool.input {
                out.push_str(&format!(" --input {}", word(&format!("{field}={source}"))));
            }
        }
        Node::Gate(gate) => {
            out.push_str(&format!(
                " --approval-schema {}",
                compact(&gate.approval_schema)
            ));
            if let Some(prompt) = &gate.prompt {
                out.push_str(&format!(" --prompt {}", word(prompt)));
            }
        }
        Node::Branch(branch) => {
            if let Some(on) = &branch.on {
                out.push_str(&format!(" --on {}", word(on)));
            }
            if let Some(hash) = &branch.agent_hash {
                out.push_str(&format!(" --hash {}", word(hash)));
            }
            // A branch's cases are never on its `add` line: `parse` cannot put
            // them there, and each arrives as its own `case` command, which
            // dumps as its own line.
        }
        Node::Map(map) => {
            out.push_str(&format!(
                " --over {} --concurrency {} {}",
                word(&map.over),
                map.concurrency,
                map_body_option(&map.body),
            ));
        }
        Node::Fold(fold) => {
            out.push_str(&format!(
                " {} --max-iterations {} --stop-when {} --join {}",
                fold_body_option(&fold.body),
                fold.max_iterations,
                word(&fold.stop_when),
                join_option(&fold.join),
            ));
            if let Some(schema) = &fold.accumulator_schema {
                out.push_str(&format!(" --accumulator-schema {}", compact(schema)));
            }
        }
    }
    if let Some(name) = node.name() {
        out.push_str(&format!(" --name {}", word(name)));
    }
    if let Some(schema) = node.input_schema() {
        out.push_str(&format!(" --input-schema {}", compact(schema)));
    }
    if let Some(schema) = node.output_schema() {
        out.push_str(&format!(" --output-schema {}", compact(schema)));
    }
    out
}

/// A map body as the option that reproduces it.
fn map_body_option(body: &MapBody) -> String {
    match body {
        MapBody::Node(id) => format!("--body {}", word(id)),
        MapBody::Subgraph(graph) => format!("--body-subgraph {}", compact_graph(graph)),
    }
}

/// A fold body as the option that reproduces it.
fn fold_body_option(body: &FoldBody) -> String {
    match body {
        FoldBody::Node(id) => format!("--body {}", word(id)),
        FoldBody::Subgraph(graph) => format!("--body-subgraph {}", compact_graph(graph)),
    }
}

/// A fold join rule as the `--join` value that reproduces it.
fn join_option(join: &FoldJoin) -> String {
    match join {
        FoldJoin::BestBy(reference) => format!("best-by:{reference}"),
        FoldJoin::Last => "last".to_owned(),
        FoldJoin::All => "all".to_owned(),
    }
}

/// A JSON value as compact single-line text. Compact, not pretty, because a
/// dumped command has to survive as one line.
fn compact(value: &Value) -> String {
    serde_json::to_string(value).expect("a JSON value is always serializable")
}

/// An embedded document as compact single-line text, for a `--body-subgraph`
/// option.
fn compact_graph(graph: &Graph) -> String {
    serde_json::to_string(graph).expect("a graph document is always serializable")
}

/// One token of a dumped line: bare when [`tokenize`] would read it back
/// whole, quoted when it would not.
fn word(text: &str) -> String {
    let plain = !text.is_empty()
        && !text.chars().any(|c| {
            c.is_whitespace() || c == '"' || c == '\\' || c == '{' || c == '[' || c == '#'
        });
    if plain { text.to_owned() } else { quote(text) }
}

/// A double-quoted token with the escapes [`tokenize`] understands. Also used
/// for display, where quoting a name or an expression marks where it ends.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

// --- reading a line ---------------------------------------------------------

/// Every command word, in the order `help` lists them.
const COMMAND_WORDS: &str = "add, case, edge, help, history, read, rm, show, undo, validate, write";

/// Every node kind word.
const KIND_WORDS: &str = "agent, tool, gate, branch, map, fold";

/// Why a line is not a command.
///
/// Structured rather than a bare string so a caller can tell a typo from a
/// missing value, and every variant names the command it was reading and the
/// exact token that broke it. A malformed line is ALWAYS one of these; nothing
/// in [`parse`] panics on input, however strange.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The line's first word is not a command.
    #[error("unknown command `{word}`; the commands are {COMMAND_WORDS}")]
    UnknownCommand {
        /// The word that was not a command.
        word: String,
    },

    /// `add` was given a word that is not one of the six node kinds.
    #[error("unknown node kind `{word}`; the kinds are {KIND_WORDS}")]
    UnknownNodeKind {
        /// The word that was not a node kind.
        word: String,
    },

    /// A required positional argument is missing.
    #[error("{command}: missing {what}")]
    MissingArgument {
        /// The command being read, as it would appear in `help`.
        command: String,
        /// What was expected, in words ("a node id").
        what: String,
    },

    /// A required option was never given.
    #[error("{command}: missing required option {option}")]
    MissingOption {
        /// The command being read.
        command: String,
        /// The option that has to be there.
        option: String,
    },

    /// An option was given with nothing after it.
    #[error("{command}: {option} needs a value")]
    MissingOptionValue {
        /// The command being read.
        command: String,
        /// The option left dangling.
        option: String,
    },

    /// Two options that cannot both be given were both given.
    #[error("{command}: {first} and {second} cannot both be given")]
    ConflictingOptions {
        /// The command being read.
        command: String,
        /// The first of the two.
        first: String,
        /// The second of the two.
        second: String,
    },

    /// A word appeared where the command expected nothing more.
    #[error("{command}: unexpected argument `{found}`")]
    UnexpectedArgument {
        /// The command being read.
        command: String,
        /// The word that had no place.
        found: String,
    },

    /// An option this command does not have.
    #[error("{command}: unknown option {option}")]
    UnknownOption {
        /// The command being read.
        command: String,
        /// The option it does not have.
        option: String,
    },

    /// An option's value has to be JSON and is not.
    #[error("{command}: {option} needs JSON: {error}")]
    MalformedJson {
        /// The command being read.
        command: String,
        /// The option whose value was not JSON.
        option: String,
        /// The JSON parser's message.
        error: String,
    },

    /// An option's value has to be a whole number and is not.
    #[error("{command}: {option} needs a whole number, found `{found}`")]
    MalformedNumber {
        /// The command being read.
        command: String,
        /// The option whose value was not a number.
        option: String,
        /// The value that was not a number.
        found: String,
    },

    /// A `--input` mapping is not `FIELD=SOURCE`.
    #[error("add tool: --input needs FIELD=SOURCE, found `{found}`")]
    MalformedInputMapping {
        /// The value that was not a mapping.
        found: String,
    },

    /// A `--join` value is not one of the three join rules.
    #[error("add fold: --join needs last, all, or best-by:<REF>, found `{found}`")]
    MalformedJoin {
        /// The value that was not a join rule.
        found: String,
    },

    /// A double-quoted token never closed.
    #[error("unterminated double quote")]
    UnterminatedQuote,

    /// A JSON argument's braces or brackets never balanced.
    #[error("unbalanced braces in a JSON argument")]
    UnbalancedJson,
}

/// Splits a line into tokens.
///
/// Three token forms, which between them cover everything a graph document
/// needs typed at it:
///
/// - a double-quoted string, understanding `\"`, `\\`, `\n`, and `\t`, for a
///   node name, a gate prompt, or a `stop_when` expression that contains
///   spaces;
/// - a run starting `{` or `[` and read until its braces balance, so a JSON
///   schema is ONE token without the author escaping the quotes inside it.
///   Strings inside the JSON are respected, so a `}` in a string value does not
///   end the token early;
/// - anything else, read to the next whitespace.
fn tokenize(line: &str) -> Result<Vec<String>, ParseError> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut at = 0;

    while at < chars.len() {
        if chars[at].is_whitespace() {
            at += 1;
            continue;
        }
        if chars[at] == '"' {
            at += 1;
            let mut text = String::new();
            let mut closed = false;
            while at < chars.len() {
                let c = chars[at];
                at += 1;
                match c {
                    '"' => {
                        closed = true;
                        break;
                    }
                    '\\' => {
                        let Some(escaped) = chars.get(at) else { break };
                        at += 1;
                        text.push(match escaped {
                            'n' => '\n',
                            't' => '\t',
                            other => *other,
                        });
                    }
                    other => text.push(other),
                }
            }
            if !closed {
                return Err(ParseError::UnterminatedQuote);
            }
            tokens.push(text);
        } else if chars[at] == '{' || chars[at] == '[' {
            let start = at;
            let mut depth = 0usize;
            let mut in_string = false;
            let mut escaped = false;
            while at < chars.len() {
                let c = chars[at];
                at += 1;
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == '"' {
                        in_string = false;
                    }
                    continue;
                }
                match c {
                    '"' => in_string = true,
                    '{' | '[' => depth += 1,
                    '}' | ']' => {
                        // Saturating because the loop is only entered on an
                        // opener, so depth is at least 1 by the time a closer
                        // is seen. Saturating keeps that reasoning from
                        // becoming a panic if it ever stops holding.
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if depth != 0 {
                return Err(ParseError::UnbalancedJson);
            }
            tokens.push(chars[start..at].iter().collect());
        } else {
            let start = at;
            while at < chars.len() && !chars[at].is_whitespace() {
                at += 1;
            }
            tokens.push(chars[start..at].iter().collect());
        }
    }

    Ok(tokens)
}

/// Walks a command's tokens, turning every way of running out of them into a
/// [`ParseError`] that names the command.
struct Scan<'a> {
    command: String,
    tokens: &'a [String],
    at: usize,
}

impl<'a> Scan<'a> {
    fn new(command: impl Into<String>, tokens: &'a [String]) -> Self {
        Self {
            command: command.into(),
            tokens,
            at: 0,
        }
    }

    /// The next token, or `None` at the end.
    fn take(&mut self) -> Option<&'a str> {
        let token = self.tokens.get(self.at)?;
        self.at += 1;
        Some(token.as_str())
    }

    /// A required positional argument, described for the error message.
    fn word(&mut self, what: &str) -> Result<String, ParseError> {
        self.take()
            .map(str::to_owned)
            .ok_or_else(|| ParseError::MissingArgument {
                command: self.command.clone(),
                what: what.to_owned(),
            })
    }

    /// An option's value.
    fn value(&mut self, option: &str) -> Result<String, ParseError> {
        self.take()
            .map(str::to_owned)
            .ok_or_else(|| ParseError::MissingOptionValue {
                command: self.command.clone(),
                option: option.to_owned(),
            })
    }

    /// An option's value, parsed as JSON.
    fn json(&mut self, option: &str) -> Result<Value, ParseError> {
        let raw = self.value(option)?;
        serde_json::from_str(&raw).map_err(|error| ParseError::MalformedJson {
            command: self.command.clone(),
            option: option.to_owned(),
            error: error.to_string(),
        })
    }

    /// An option's value, parsed as a whole number.
    fn number(&mut self, option: &str) -> Result<u32, ParseError> {
        let raw = self.value(option)?;
        raw.parse().map_err(|_| ParseError::MalformedNumber {
            command: self.command.clone(),
            option: option.to_owned(),
            found: raw,
        })
    }

    /// An option's value, parsed as an embedded document.
    fn subgraph(&mut self, option: &str) -> Result<Graph, ParseError> {
        let raw = self.value(option)?;
        serde_json::from_str(&raw).map_err(|error| ParseError::MalformedJson {
            command: self.command.clone(),
            option: option.to_owned(),
            error: error.to_string(),
        })
    }

    /// The error a token that is neither a known option nor an expected
    /// positional earns: an unknown option when it looks like one, a stray
    /// argument otherwise.
    fn stray(&self, found: &str) -> ParseError {
        if found.starts_with("--") {
            ParseError::UnknownOption {
                command: self.command.clone(),
                option: found.to_owned(),
            }
        } else {
            ParseError::UnexpectedArgument {
                command: self.command.clone(),
                found: found.to_owned(),
            }
        }
    }

    /// Refuses a missing required option.
    fn missing(&self, option: &str) -> ParseError {
        ParseError::MissingOption {
            command: self.command.clone(),
            option: option.to_owned(),
        }
    }

    /// Refuses two options that cannot both appear.
    fn conflict(&self, first: &str, second: &str) -> ParseError {
        ParseError::ConflictingOptions {
            command: self.command.clone(),
            first: first.to_owned(),
            second: second.to_owned(),
        }
    }
}

/// Reads one line of editor input.
///
/// `Ok(None)` means the line was blank or a `#` comment, which is not an error:
/// a script dumped by [`Editor::script`] can be annotated and still replay.
///
/// # Errors
///
/// Returns the [`ParseError`] naming the command being read and the token that
/// broke it. Every malformed line lands here; none panics.
pub fn parse(line: &str) -> Result<Option<Line>, ParseError> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }

    let tokens = tokenize(line)?;
    let Some((head, rest)) = tokens.split_first() else {
        return Ok(None);
    };

    let line = match head.as_str() {
        "add" => parse_add(rest)?,
        "edge" => Line::Command(parse_edge(rest)?),
        "case" => Line::Command(parse_case(rest)?),
        "rm" => Line::Command(parse_remove(rest)?),
        "show" => Line::Command(parse_show(rest)?),
        "validate" => Line::Command(parse_bare("validate", rest, Command::Validate)?),
        "read" => parse_read(rest)?,
        "write" => parse_write(rest)?,
        "undo" => Line::Command(parse_bare("undo", rest, Command::Undo)?),
        "history" => Line::Command(parse_bare("history", rest, Command::History)?),
        "help" => Line::Command(parse_help(rest)?),
        word => {
            return Err(ParseError::UnknownCommand {
                word: word.to_owned(),
            });
        }
    };
    Ok(Some(line))
}

/// A command that takes nothing, refusing anything typed after it rather than
/// silently ignoring it.
fn parse_bare(command: &str, tokens: &[String], value: Command) -> Result<Command, ParseError> {
    let mut scan = Scan::new(command, tokens);
    if let Some(found) = scan.take() {
        return Err(scan.stray(found));
    }
    Ok(value)
}

/// `add <KIND> ...`, dispatching to one parser per node kind.
fn parse_add(tokens: &[String]) -> Result<Line, ParseError> {
    let mut scan = Scan::new("add", tokens);
    let kind = scan.word("a node kind")?;
    let rest = &tokens[1..];
    match kind.as_str() {
        "agent" => parse_add_agent(rest),
        "tool" => parse_add_tool(rest).map(Line::Command),
        "gate" => parse_add_gate(rest).map(Line::Command),
        "branch" => parse_add_branch(rest),
        "map" => parse_add_map(rest).map(Line::Command),
        "fold" => parse_add_fold(rest).map(Line::Command),
        word => Err(ParseError::UnknownNodeKind {
            word: word.to_owned(),
        }),
    }
}

/// `add agent <ID> (--hash <HASH> | --file <PATH>) [options]`.
///
/// `--file` is the whole reason [`Line::Host`] exists: the hash it stands for
/// cannot be computed in this crate, so the line comes back as a
/// [`HostRequest::AgentFile`] carrying the rest of the node already built.
fn parse_add_agent(tokens: &[String]) -> Result<Line, ParseError> {
    let mut scan = Scan::new("add agent", tokens);
    let id = scan.word("a node id")?;
    let mut hash = None;
    let mut file = None;
    let mut name = None;
    let mut input_schema = None;
    let mut output_schema = None;

    while let Some(token) = scan.take() {
        match token {
            "--hash" => hash = Some(scan.value("--hash")?),
            "--file" => file = Some(scan.value("--file")?),
            "--name" => name = Some(scan.value("--name")?),
            "--input-schema" => input_schema = Some(scan.json("--input-schema")?),
            "--output-schema" => output_schema = Some(scan.json("--output-schema")?),
            found => return Err(scan.stray(found)),
        }
    }

    if hash.is_some() && file.is_some() {
        return Err(scan.conflict("--hash", "--file"));
    }
    let node = Node::Agent(AgentNode {
        id,
        // Empty only on the `--file` path, where `HashDraft::with_hash` is the
        // only way the node can reach a document.
        agent_hash: hash.clone().unwrap_or_default(),
        name,
        input_schema,
        output_schema,
    });
    match (hash, file) {
        (Some(_), _) => Ok(Line::Command(Command::Add(Box::new(node)))),
        (None, Some(path)) => Ok(Line::Host(HostRequest::AgentFile {
            path,
            draft: HashDraft { node },
        })),
        (None, None) => Err(scan.missing("--hash or --file")),
    }
}

/// `add tool <ID> <TOOL> [--input FIELD=SOURCE ...] [options]`.
fn parse_add_tool(tokens: &[String]) -> Result<Command, ParseError> {
    let mut scan = Scan::new("add tool", tokens);
    let id = scan.word("a node id")?;
    let tool = scan.word("a tool name")?;
    let mut name = None;
    let mut input = BTreeMap::new();
    let mut input_schema = None;
    let mut output_schema = None;

    while let Some(token) = scan.take() {
        match token {
            "--name" => name = Some(scan.value("--name")?),
            "--input" => {
                let mapping = scan.value("--input")?;
                let Some((field, source)) = mapping.split_once('=') else {
                    return Err(ParseError::MalformedInputMapping { found: mapping });
                };
                input.insert(field.to_owned(), source.to_owned());
            }
            "--input-schema" => input_schema = Some(scan.json("--input-schema")?),
            "--output-schema" => output_schema = Some(scan.json("--output-schema")?),
            found => return Err(scan.stray(found)),
        }
    }

    Ok(Command::Add(Box::new(Node::Tool(ToolNode {
        id,
        tool,
        name,
        input,
        input_schema,
        output_schema,
    }))))
}

/// `add gate <ID> --approval-schema <JSON> [--prompt TEXT] [--name TEXT]`.
///
/// The approval schema is required because the model makes it required: a gate
/// with no declared approval shape is meaningless.
fn parse_add_gate(tokens: &[String]) -> Result<Command, ParseError> {
    let mut scan = Scan::new("add gate", tokens);
    let id = scan.word("a node id")?;
    let mut name = None;
    let mut prompt = None;
    let mut approval_schema = None;

    while let Some(token) = scan.take() {
        match token {
            "--name" => name = Some(scan.value("--name")?),
            "--prompt" => prompt = Some(scan.value("--prompt")?),
            "--approval-schema" => approval_schema = Some(scan.json("--approval-schema")?),
            found => return Err(scan.stray(found)),
        }
    }

    let approval_schema = approval_schema.ok_or_else(|| scan.missing("--approval-schema"))?;
    Ok(Command::Add(Box::new(Node::Gate(GateNode {
        id,
        name,
        prompt,
        approval_schema,
    }))))
}

/// `add branch <ID> [--on REF] [--hash HASH | --file PATH] [--name TEXT]`.
///
/// A branch is added with NO cases; each one arrives as its own `case` command.
/// That keeps a line short and makes a case something you can add and remove
/// without rewriting the node.
fn parse_add_branch(tokens: &[String]) -> Result<Line, ParseError> {
    let mut scan = Scan::new("add branch", tokens);
    let id = scan.word("a node id")?;
    let mut name = None;
    let mut on = None;
    let mut hash = None;
    let mut file = None;

    while let Some(token) = scan.take() {
        match token {
            "--name" => name = Some(scan.value("--name")?),
            "--on" => on = Some(scan.value("--on")?),
            "--hash" => hash = Some(scan.value("--hash")?),
            "--file" => file = Some(scan.value("--file")?),
            found => return Err(scan.stray(found)),
        }
    }

    if hash.is_some() && file.is_some() {
        return Err(scan.conflict("--hash", "--file"));
    }
    let node = Node::Branch(BranchNode {
        id,
        name,
        on,
        agent_hash: hash,
        cases: Vec::new(),
    });
    match file {
        Some(path) => Ok(Line::Host(HostRequest::AgentFile {
            path,
            draft: HashDraft { node },
        })),
        None => Ok(Line::Command(Command::Add(Box::new(node)))),
    }
}

/// `add map <ID> --over REF --concurrency N (--body ID | --body-subgraph JSON)`.
fn parse_add_map(tokens: &[String]) -> Result<Command, ParseError> {
    let mut scan = Scan::new("add map", tokens);
    let id = scan.word("a node id")?;
    let mut name = None;
    let mut over = None;
    let mut concurrency = None;
    let mut body = None;
    let mut output_schema = None;

    while let Some(token) = scan.take() {
        match token {
            "--name" => name = Some(scan.value("--name")?),
            "--over" => over = Some(scan.value("--over")?),
            "--concurrency" => concurrency = Some(scan.number("--concurrency")?),
            "--body" => body = Some(MapBody::Node(scan.value("--body")?)),
            "--body-subgraph" => {
                body = Some(MapBody::Subgraph(Box::new(
                    scan.subgraph("--body-subgraph")?,
                )));
            }
            "--output-schema" => output_schema = Some(scan.json("--output-schema")?),
            found => return Err(scan.stray(found)),
        }
    }

    Ok(Command::Add(Box::new(Node::Map(MapNode {
        id,
        name,
        over: over.ok_or_else(|| scan.missing("--over"))?,
        // Not defaulted to 1: a cap the author did not choose is a cap nobody
        // reviews, and `validate` is the one that judges the number anyway.
        concurrency: concurrency.ok_or_else(|| scan.missing("--concurrency"))?,
        body: body.ok_or_else(|| scan.missing("--body or --body-subgraph"))?,
        output_schema,
    }))))
}

/// `add fold <ID> --body ID --max-iterations N --stop-when EXPR --join J`.
///
/// The `stop_when` expression is NOT parsed here. [`salvor_graph::validate`]
/// parses it and reports a bad one node-precise, and having two places that
/// judge an expression is exactly the duplication this module avoids.
fn parse_add_fold(tokens: &[String]) -> Result<Command, ParseError> {
    let mut scan = Scan::new("add fold", tokens);
    let id = scan.word("a node id")?;
    let mut name = None;
    let mut body = None;
    let mut max_iterations = None;
    let mut stop_when = None;
    let mut join = None;
    let mut accumulator_schema = None;

    while let Some(token) = scan.take() {
        match token {
            "--name" => name = Some(scan.value("--name")?),
            "--body" => body = Some(FoldBody::Node(scan.value("--body")?)),
            "--body-subgraph" => {
                body = Some(FoldBody::Subgraph(Box::new(
                    scan.subgraph("--body-subgraph")?,
                )));
            }
            "--max-iterations" => max_iterations = Some(scan.number("--max-iterations")?),
            "--stop-when" => stop_when = Some(scan.value("--stop-when")?),
            "--join" => {
                let raw = scan.value("--join")?;
                join = Some(parse_join(&raw)?);
            }
            "--accumulator-schema" => {
                accumulator_schema = Some(scan.json("--accumulator-schema")?);
            }
            found => return Err(scan.stray(found)),
        }
    }

    Ok(Command::Add(Box::new(Node::Fold(FoldNode {
        id,
        name,
        body: body.ok_or_else(|| scan.missing("--body or --body-subgraph"))?,
        max_iterations: max_iterations.ok_or_else(|| scan.missing("--max-iterations"))?,
        stop_when: stop_when.ok_or_else(|| scan.missing("--stop-when"))?,
        join: join.ok_or_else(|| scan.missing("--join"))?,
        accumulator_schema,
    }))))
}

/// A `--join` value. One token by construction, so `best-by:score` needs no
/// quoting and the three rules cannot be confused for one another.
fn parse_join(raw: &str) -> Result<FoldJoin, ParseError> {
    match raw {
        "last" => Ok(FoldJoin::Last),
        "all" => Ok(FoldJoin::All),
        _ => match raw.strip_prefix("best-by:") {
            Some(reference) if !reference.is_empty() => Ok(FoldJoin::BestBy(reference.to_owned())),
            _ => Err(ParseError::MalformedJoin {
                found: raw.to_owned(),
            }),
        },
    }
}

/// `edge <FROM> <TO> [--label NAME]`.
fn parse_edge(tokens: &[String]) -> Result<Command, ParseError> {
    let mut scan = Scan::new("edge", tokens);
    let from = scan.word("a source node id")?;
    let to = scan.word("a destination node id")?;
    let mut label = None;
    while let Some(token) = scan.take() {
        match token {
            "--label" => label = Some(scan.value("--label")?),
            found => return Err(scan.stray(found)),
        }
    }
    Ok(Command::Edge(Edge { from, to, label }))
}

/// `case <BRANCH-ID> <CASE-NAME> (--when EXPR | --model)`.
fn parse_case(tokens: &[String]) -> Result<Command, ParseError> {
    let mut scan = Scan::new("case", tokens);
    let node = scan.word("a branch node id")?;
    let name = scan.word("a case name")?;
    let mut expression = None;
    let mut model = false;
    while let Some(token) = scan.take() {
        match token {
            "--when" => expression = Some(scan.value("--when")?),
            "--model" => model = true,
            found => return Err(scan.stray(found)),
        }
    }
    let when = match (expression, model) {
        (Some(_), true) => return Err(scan.conflict("--when", "--model")),
        (Some(expression), false) => BranchCondition::Expression(expression),
        (None, true) => BranchCondition::ModelDecision,
        (None, false) => return Err(scan.missing("--when or --model")),
    };
    Ok(Command::Case {
        node,
        case: BranchCase { name, when },
    })
}

/// `rm node <ID>`, `rm edge <FROM> <TO> [--label NAME]`, or
/// `rm case <BRANCH-ID> <CASE-NAME>`.
///
/// The kind word is required rather than inferred from the id, because a node
/// and an edge are different things and guessing which one `rm x` meant would
/// eventually guess wrong.
fn parse_remove(tokens: &[String]) -> Result<Command, ParseError> {
    let mut scan = Scan::new("rm", tokens);
    let what = scan.word("node, edge, or case")?;
    let rest = &tokens[1..];
    match what.as_str() {
        "node" => {
            let mut scan = Scan::new("rm node", rest);
            let id = scan.word("a node id")?;
            if let Some(found) = scan.take() {
                return Err(scan.stray(found));
            }
            Ok(Command::Remove(Target::Node { id }))
        }
        "edge" => {
            let mut scan = Scan::new("rm edge", rest);
            let from = scan.word("a source node id")?;
            let to = scan.word("a destination node id")?;
            let mut label = None;
            while let Some(token) = scan.take() {
                match token {
                    "--label" => label = Some(scan.value("--label")?),
                    found => return Err(scan.stray(found)),
                }
            }
            Ok(Command::Remove(Target::Edge { from, to, label }))
        }
        "case" => {
            let mut scan = Scan::new("rm case", rest);
            let node = scan.word("a branch node id")?;
            let case = scan.word("a case name")?;
            if let Some(found) = scan.take() {
                return Err(scan.stray(found));
            }
            Ok(Command::Remove(Target::Case { node, case }))
        }
        found => Err(ParseError::UnexpectedArgument {
            command: "rm".to_owned(),
            found: found.to_owned(),
        }),
    }
}

/// `show` or `show <ID>`.
fn parse_show(tokens: &[String]) -> Result<Command, ParseError> {
    let mut scan = Scan::new("show", tokens);
    let node = scan.take().map(str::to_owned);
    if let Some(found) = scan.take() {
        return Err(scan.stray(found));
    }
    Ok(Command::Show { node })
}

/// `read <PATH>` or `read --json <JSON>`.
///
/// The two forms are the seam, stated in the grammar rather than inferred: the
/// `--json` form is complete and applies straight away, and it is the form
/// [`Command::to_line`] always dumps, so a script never needs a file. The path
/// form is a [`HostRequest::ReadFile`] the caller resolves.
fn parse_read(tokens: &[String]) -> Result<Line, ParseError> {
    let mut scan = Scan::new("read", tokens);
    let first = scan.word("a path, or --json with a document")?;
    if first == "--json" {
        let json = scan.value("--json")?;
        if let Some(found) = scan.take() {
            return Err(scan.stray(found));
        }
        return Ok(Line::Command(Command::Read { json }));
    }
    if first.starts_with("--") {
        return Err(scan.stray(&first));
    }
    if let Some(found) = scan.take() {
        return Err(scan.stray(found));
    }
    Ok(Line::Host(HostRequest::ReadFile { path: first }))
}

/// `write` or `write <PATH>`.
///
/// Bare `write` hands the JSON back in [`Outcome::document_json`] for the
/// caller to do as it likes with, which is what a browser terminal with no
/// filesystem uses. With a path it is a [`HostRequest::WriteFile`].
fn parse_write(tokens: &[String]) -> Result<Line, ParseError> {
    let mut scan = Scan::new("write", tokens);
    let path = scan.take().map(str::to_owned);
    if let Some(found) = scan.take() {
        return Err(scan.stray(found));
    }
    match path {
        Some(path) if path.starts_with("--") => Err(scan.stray(&path)),
        Some(path) => Ok(Line::Host(HostRequest::WriteFile { path })),
        None => Ok(Line::Command(Command::Write)),
    }
}

/// `help` or `help <COMMAND>`.
fn parse_help(tokens: &[String]) -> Result<Command, ParseError> {
    let mut scan = Scan::new("help", tokens);
    let topic = scan.take().map(str::to_owned);
    if let Some(found) = scan.take() {
        return Err(scan.stray(found));
    }
    Ok(Command::Help { topic })
}

// --- the grammar, as the editor explains it ---------------------------------

/// One command's help entry.
struct Topic {
    /// The command word, which is also the `help <TOPIC>` argument.
    name: &'static str,
    /// The one-line summary the overview lists.
    summary: &'static str,
    /// The grammar forms, printed verbatim.
    forms: &'static [&'static str],
    /// The paragraph explaining what the command means, wrapped to the pane.
    detail: &'static str,
}

/// Every command's help, in the order the overview lists them: the shaping
/// commands first, then the queries, then the two that read and write a whole
/// document, then `undo`, `history`, and `help`.
///
/// This table is the one place the grammar is written down for a reader, and it
/// sits beside the parser it describes so the two are edited together.
const TOPICS: &[Topic] = &[
    Topic {
        name: "add",
        summary: "add a node of one of the six kinds",
        forms: &[
            "add agent  <ID> (--hash <sha256:HASH> | --file <PATH>) [--name TEXT] [--input-schema JSON] [--output-schema JSON]",
            "add tool   <ID> <TOOL> [--input FIELD=SOURCE]... [--name TEXT] [--input-schema JSON] [--output-schema JSON]",
            "add gate   <ID> --approval-schema JSON [--prompt TEXT] [--name TEXT]",
            "add branch <ID> [--on REF] [--hash <sha256:HASH> | --file <PATH>] [--name TEXT]",
            "add map    <ID> --over REF --concurrency N (--body ID | --body-subgraph JSON) [--output-schema JSON] [--name TEXT]",
            "add fold   <ID> (--body ID | --body-subgraph JSON) --max-iterations N --stop-when EXPR --join (last | all | best-by:REF) [--accumulator-schema JSON] [--name TEXT]",
        ],
        detail: "An agent node names its definition by content hash, never by path, because a \
                 run records only the hash. `--hash` takes one you already have; `--file` asks \
                 the host to compute it from an agent definition, which this editor cannot do \
                 itself. A branch is added with no cases; use `case` to add them one at a time. \
                 A JSON argument needs no quoting: braces are read until they balance.",
    },
    Topic {
        name: "edge",
        summary: "connect two nodes",
        forms: &["edge <FROM> <TO> [--label NAME]"],
        detail: "Edges are the only topology in a document. An endpoint that does not exist yet \
                 is allowed, so you can sketch the shape before filling it in; validate names \
                 the dangling endpoint until you do. A label on an edge out of a branch names \
                 the case it realizes.",
    },
    Topic {
        name: "case",
        summary: "add a condition to a branch node",
        forms: &[
            "case <BRANCH-ID> <CASE-NAME> --when EXPR",
            "case <BRANCH-ID> <CASE-NAME> --model",
        ],
        detail: "`--when` takes a condition expression over the routed value; `--model` says a \
                 model decides this case at run time, which needs the branch to carry an agent \
                 hash. The expression is not checked here: validate parses it and names the \
                 branch and the case if it is malformed. An edge labeled with the case name is \
                 what actually routes it.",
    },
    Topic {
        name: "rm",
        summary: "remove a node, an edge, or a branch case",
        forms: &[
            "rm node <ID>",
            "rm edge <FROM> <TO> [--label NAME]",
            "rm case <BRANCH-ID> <CASE-NAME>",
        ],
        detail: "Removing a node removes every edge that names it, because an edge cannot mean \
                 anything without both endpoints. A map or fold body that names the node is \
                 left alone, and validate reports it. `rm edge` with no label removes every \
                 edge between the two nodes; with one, only the edges carrying it.",
    },
    Topic {
        name: "show",
        summary: "read the document back, whole or one node",
        forms: &["show", "show <ID>"],
        detail: "Bare `show` is the outline: one line per node with its distinguishing fields, \
                 then the edge list. Hashes are shortened and a schema is reduced to its type, \
                 because the outline is for checking the shape. `show <ID>` prints one node in \
                 full, with the whole hash and every schema pretty-printed.",
    },
    Topic {
        name: "validate",
        summary: "run every check and list every problem",
        forms: &["validate"],
        detail: "Runs the same validator `salvor graph validate` runs, which collects EVERY \
                 failure rather than stopping at the first and names the node or edge at fault \
                 in each. Nothing else in this editor validates, so a document is never \
                 refused for being incomplete: build it, then ask.",
    },
    Topic {
        name: "read",
        summary: "replace the document with one that already exists",
        forms: &["read <PATH>", "read --json <JSON>"],
        detail: "Replaces the whole document. It is recorded like any other command, so `undo` \
                 straight after a read brings back exactly what you had before it. A document \
                 that does not validate reads in fine; only one that does not PARSE is \
                 refused, and then the message names the offending field.",
    },
    Topic {
        name: "write",
        summary: "serialize the document as JSON",
        forms: &["write", "write <PATH>"],
        detail: "Produces the document's JSON. With a path the host writes it there; bare, the \
                 JSON comes back to whatever is driving the editor. Writing does not validate \
                 first, so an unfinished document can be saved and picked up later.",
    },
    Topic {
        name: "undo",
        summary: "take back the last command that changed the document",
        forms: &["undo"],
        detail: "Drops the last recorded command and rebuilds the document from the ones that \
                 remain. Because the document is derived rather than patched, undo needs no \
                 inverse for anything: taking back a removal restores what it removed, and \
                 taking back a read restores the document the read replaced.",
    },
    Topic {
        name: "history",
        summary: "dump the session as a replayable script",
        forms: &["history"],
        detail: "Prints one line per recorded command, which is exactly the script that \
                 rebuilds this document. Queries are not recorded, and every line is in its \
                 resolved form, so the script replays with no files present.",
    },
    Topic {
        name: "help",
        summary: "list the commands, or explain one",
        forms: &["help", "help <COMMAND>"],
        detail: "Bare `help` lists every command with a one-line summary. `help <COMMAND>` \
                 prints that command's grammar and what it means.",
    },
];

/// The `help` output.
///
/// The grammar forms are printed VERBATIM, never wrapped, for the same reason
/// [`crate::render`] never wraps a command a reader is meant to copy: a form
/// broken across lines is a form you cannot type. Only the prose reflows.
fn help(topic: Option<&str>, width: usize) -> Result<String, String> {
    let Some(topic) = topic else {
        let mut out = wrap(
            "A graph document, one line at a time. Each command below changes the document or \
             reads it back; `help <COMMAND>` prints one command's full grammar.",
            width,
            "",
            "",
        );
        out.push_str("\n\n");
        for entry in TOPICS {
            out.push_str(&wrap(
                entry.summary,
                width,
                &format!("  {:<9} ", entry.name),
                "            ",
            ));
            out.push('\n');
        }
        return Ok(out);
    };

    let Some(entry) = TOPICS.iter().find(|entry| entry.name == topic) else {
        return Err(format!(
            "no help for `{topic}`; the commands are {COMMAND_WORDS}"
        ));
    };
    let mut out = format!("{}: {}\n\n", entry.name, entry.summary);
    for form in entry.forms {
        out.push_str(&format!("  {form}\n"));
    }
    out.push('\n');
    out.push_str(&wrap(entry.detail, width, "", ""));
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::DEFAULT_REPORT_WIDTH as WIDTH;

    /// Applies a script line by line, refusing to let a parse error, a host
    /// request, or a refusal pass silently. This is the whole test harness: no
    /// terminal, no prompt loop, no file.
    fn run(script: &str) -> Editor {
        let mut editor = Editor::new();
        for line in script.lines() {
            let parsed =
                parse(line).unwrap_or_else(|error| panic!("line `{line}` did not parse: {error}"));
            let Some(parsed) = parsed else { continue };
            let Line::Command(command) = parsed else {
                panic!("line `{line}` unexpectedly needs the host");
            };
            let (next, outcome) = editor.apply(command, WIDTH);
            assert_ne!(
                outcome.status,
                Status::Refused,
                "line `{line}` was refused: {}",
                outcome.text
            );
            editor = next;
        }
        editor
    }

    /// Applies one line to an editor and hands back both halves, for a test
    /// that cares about the outcome as much as the document.
    fn step(editor: Editor, line: &str) -> (Editor, Outcome) {
        let parsed = parse(line)
            .unwrap_or_else(|error| panic!("line `{line}` did not parse: {error}"))
            .unwrap_or_else(|| panic!("line `{line}` was blank"));
        let Line::Command(command) = parsed else {
            panic!("line `{line}` unexpectedly needs the host");
        };
        editor.apply(command, WIDTH)
    }

    const HASH_A: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const HASH_B: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    /// The canonical cross-language flow, typed as lines. The same document the
    /// Rust, TypeScript, and Python builders are all pinned to, which is the
    /// strongest available statement that the editor authors REAL documents and
    /// not a dialect of its own.
    fn canonical_script() -> String {
        let draft =
            r#"{"type":"object","properties":{"draft":{"type":"string"}},"required":["draft"]}"#;
        let approval = r#"{"type":"object","properties":{"approved":{"type":"boolean"}},"required":["approved"]}"#;
        format!(
            "# the research, review, approve, publish flow\n\
             add agent research --hash {HASH_A} --output-schema {draft}\n\
             add agent review --hash {HASH_B} --input-schema {draft} --output-schema {draft}\n\
             add gate approve --approval-schema {approval} --prompt \"Approve this draft for publication?\"\n\
             \n\
             add tool publish http_post --input body=approve.draft --input url=config.publish_url\n\
             edge research review\n\
             edge review approve\n\
             edge approve publish\n"
        )
    }

    /// A graph built from zero as a command list is byte for byte the canonical
    /// fixture. Nothing in the editor invents a format.
    #[test]
    fn builds_the_canonical_document_from_zero() {
        let editor = run(&canonical_script());
        let built = serde_json::to_value(editor.document()).expect("serialize");
        let canonical: Value = serde_json::from_str(include_str!(
            "../../../examples/graphs/research-review-publish.json"
        ))
        .expect("parse canonical fixture");
        assert_eq!(
            built, canonical,
            "a document typed as lines must equal the canonical fixture exactly"
        );

        let summary =
            salvor_graph::validate(editor.document()).expect("the canonical flow is valid");
        assert_eq!(summary.entry_nodes, ["research"]);
        assert_eq!(summary.terminal_nodes, ["publish"]);
    }

    /// All six node kinds are reachable from a line, and each lands as the
    /// right variant with the right payload.
    #[test]
    fn every_node_kind_is_addable_from_a_line() {
        let editor = run(&format!(
            "add agent research --hash {HASH_A} --name \"Research the topic\"\n\
             add tool publish http_post --input body=approve.draft\n\
             add gate approve --approval-schema {{\"type\":\"object\"}} --prompt \"Ship it?\"\n\
             add branch route --on score.value --hash {HASH_B}\n\
             add map fanout --over route.items --concurrency 4 --body research\n\
             add fold refine --body research --max-iterations 3 --stop-when \"score >= 0.85\" \
             --join best-by:score --accumulator-schema {{\"type\":\"object\"}}\n"
        ));

        let kinds: Vec<&str> = editor
            .document()
            .nodes
            .iter()
            .map(Node::kind_name)
            .collect();
        assert_eq!(kinds, ["agent", "tool", "gate", "branch", "map", "fold"]);

        let Node::Agent(agent) = &editor.document().nodes[0] else {
            panic!("first node is an agent");
        };
        assert_eq!(agent.agent_hash, HASH_A);
        assert_eq!(agent.name.as_deref(), Some("Research the topic"));

        let Node::Tool(tool) = &editor.document().nodes[1] else {
            panic!("second node is a tool");
        };
        assert_eq!(tool.tool, "http_post");
        assert_eq!(
            tool.input.get("body").map(String::as_str),
            Some("approve.draft")
        );

        let Node::Map(map) = &editor.document().nodes[4] else {
            panic!("fifth node is a map");
        };
        assert_eq!(map.concurrency, 4);
        assert_eq!(map.body, MapBody::Node("research".to_owned()));

        let Node::Fold(fold) = &editor.document().nodes[5] else {
            panic!("sixth node is a fold");
        };
        assert_eq!(fold.max_iterations, 3);
        assert_eq!(fold.stop_when, "score >= 0.85");
        assert_eq!(fold.join, FoldJoin::BestBy("score".to_owned()));
    }

    /// A `case` command mutates an existing branch node, and the cases arrive
    /// in the order they were typed, because the first matching case in author
    /// order is the one that wins at run time.
    #[test]
    fn cases_land_on_a_branch_in_author_order() {
        let editor = run(&format!(
            "add branch route --on score.value --hash {HASH_A}\n\
             case route high --when \"score > 0.8\"\n\
             case route ask --model\n"
        ));
        let Node::Branch(branch) = &editor.document().nodes[0] else {
            panic!("the only node is a branch");
        };
        assert_eq!(
            branch.cases,
            vec![
                BranchCase {
                    name: "high".to_owned(),
                    when: BranchCondition::Expression("score > 0.8".to_owned()),
                },
                BranchCase {
                    name: "ask".to_owned(),
                    when: BranchCondition::ModelDecision,
                },
            ]
        );
    }

    /// The document is ALWAYS exactly the fold of the recorded history. This is
    /// the invariant the cached `document` field could otherwise drift from, so
    /// it is asserted after every command of a session that adds, connects,
    /// mutates, removes, and queries.
    #[test]
    fn the_document_is_always_the_fold_of_the_history() {
        let lines = [
            format!("add agent research --hash {HASH_A}"),
            format!("add branch route --hash {HASH_B}"),
            "case route high --when \"score > 0.8\"".to_owned(),
            "edge research route".to_owned(),
            "show".to_owned(),
            "validate".to_owned(),
            "rm case route high".to_owned(),
            "rm edge research route".to_owned(),
            "rm node research".to_owned(),
        ];
        let mut editor = Editor::new();
        for line in &lines {
            let (next, _) = step(editor, line);
            editor = next;
            assert_eq!(
                editor.document(),
                &Editor::from_history(editor.history().to_vec())
                    .document()
                    .clone(),
                "the cached document drifted from the fold after `{line}`"
            );
        }
    }

    /// A query never enters the history, so a dumped script is exactly the
    /// commands that shaped the document.
    #[test]
    fn queries_do_not_enter_the_history() {
        let editor = run(&format!(
            "add agent research --hash {HASH_A}\n\
             show\n\
             show research\n\
             validate\n\
             write\n\
             history\n\
             help\n\
             help add\n"
        ));
        assert_eq!(editor.history().len(), 1, "only the add is recorded");
        assert_eq!(
            editor.script(),
            format!("add agent research --hash {HASH_A}\n")
        );
    }

    // --- a partial document is a legal document -----------------------------

    /// A document that does not yet validate is a legal state, and the editor
    /// says so rather than refusing the edit. Every intermediate step here is
    /// invalid in a different way, and each is accepted; the same lines that
    /// finish the document then make it valid, with nothing retracted.
    #[test]
    fn a_document_that_does_not_validate_is_a_legal_intermediate_state() {
        let mut editor = Editor::new();

        // An edge drawn before either endpoint exists.
        let (next, outcome) = step(editor, "edge research review");
        assert_eq!(outcome.status, Status::Ok, "{}", outcome.text);
        assert!(
            outcome.text.contains("note:"),
            "the note has to say the endpoints are missing: {}",
            outcome.text
        );
        editor = next;
        assert_eq!(editor.document().edges.len(), 1);

        // A branch with no cases, and a placeholder where a hash belongs.
        let (next, outcome) = step(editor, "add branch route");
        assert_eq!(outcome.status, Status::Ok);
        editor = next;
        let (next, outcome) = step(editor, "add agent research --hash not-a-hash");
        assert_eq!(outcome.status, Status::Ok);
        editor = next;

        // A map whose concurrency cap is illegal.
        let (next, outcome) = step(
            editor,
            "add map fanout --over route.items --concurrency 0 --body research",
        );
        assert_eq!(outcome.status, Status::Ok);
        editor = next;

        // Every one of those is in the document, and validate says why.
        assert_eq!(editor.document().nodes.len(), 3);
        let (editor, outcome) = step(editor, "validate");
        assert_eq!(
            outcome.status,
            Status::Invalid,
            "an incomplete document reports Invalid, not Refused: {}",
            outcome.text
        );

        // Finishing it makes it valid, with nothing taken back.
        let editor = run_on(
            editor,
            &format!(
                "rm node fanout\n\
                 rm node research\n\
                 add agent research --hash {HASH_A}\n\
                 add agent review --hash {HASH_B}\n\
                 edge research review\n\
                 rm node route\n"
            ),
        );
        let (_, outcome) = step(editor, "validate");
        assert_eq!(outcome.status, Status::Ok, "{}", outcome.text);
    }

    /// Applies a script to an editor that already has a document.
    fn run_on(editor: Editor, script: &str) -> Editor {
        let mut editor = editor;
        for line in script.lines() {
            let (next, outcome) = step(editor, line);
            assert_ne!(
                outcome.status,
                Status::Refused,
                "line `{line}` was refused: {}",
                outcome.text
            );
            editor = next;
        }
        editor
    }

    /// `validate` surfaces EVERY problem at once, straight from
    /// `salvor_graph::validate`, each naming the node or edge at fault. The
    /// editor writes no validator of its own, so this is a test that the real
    /// one is wired through rather than a test of the rules themselves.
    #[test]
    fn validate_surfaces_every_problem_at_once() {
        let editor = run("add agent research --hash nope\n\
             add map fanout --over route.items --concurrency 0 --body ghost\n\
             add gate approve --approval-schema [1,2,3]\n\
             add branch route\n\
             case route ask --model\n\
             case route bad --when \"score >>> 0.8\"\n\
             edge research missing\n");

        let errors = salvor_graph::validate(editor.document())
            .expect_err("this document has several problems");
        assert!(
            errors.len() >= 6,
            "the real validator collects every failure, found {}: {errors:?}",
            errors.len()
        );

        let (_, outcome) = step(editor, "validate");
        assert_eq!(outcome.status, Status::Invalid);
        assert!(
            outcome
                .text
                .starts_with(&format!("graph invalid: {} problem(s)", errors.len())),
            "the report opens with the count: {}",
            outcome.text
        );
        // Every problem is listed, and each names its node or edge.
        for expected in [
            "agent node `research`",
            "map node `fanout`",
            "gate node `approve`",
            "branch node `route`",
            "case `bad`",
            "unknown node id `missing`",
        ] {
            assert!(
                outcome.text.contains(expected),
                "the report must name {expected}:\n{}",
                outcome.text
            );
        }
        for number in 1..=errors.len() {
            assert!(
                outcome.text.contains(&format!("{number}. ")),
                "problem {number} is numbered:\n{}",
                outcome.text
            );
        }
    }

    // --- undo ---------------------------------------------------------------

    /// `undo` steps back exactly one command, and repeated undos walk the
    /// session back to empty. No command carries an inverse: the document is
    /// re-derived from the commands that remain.
    #[test]
    fn undo_steps_back_one_command_at_a_time() {
        let editor = run(&format!(
            "add agent research --hash {HASH_A}\n\
             add agent review --hash {HASH_B}\n\
             edge research review\n"
        ));
        assert_eq!(editor.document().nodes.len(), 2);
        assert_eq!(editor.document().edges.len(), 1);

        let (editor, outcome) = step(editor, "undo");
        assert_eq!(outcome.status, Status::Ok);
        assert!(
            outcome.text.contains("undid `edge research review`"),
            "undo names the exact line it dropped: {}",
            outcome.text
        );
        assert_eq!(editor.document().edges.len(), 0, "the edge is gone");
        assert_eq!(editor.document().nodes.len(), 2, "the nodes are not");

        let (editor, _) = step(editor, "undo");
        let (editor, _) = step(editor, "undo");
        assert_eq!(editor.document(), &empty_document(), "back to empty");
        assert!(editor.history().is_empty());

        let (_, outcome) = step(editor, "undo");
        assert_eq!(
            outcome.status,
            Status::Refused,
            "undoing past the beginning is refused, not silently ignored"
        );
        assert!(outcome.text.contains("nothing to undo"));
    }

    /// `undo` after a removal restores what was removed, including the incident
    /// edges the removal took with it. This is the payoff of deriving rather
    /// than patching: a removal never had to remember anything.
    #[test]
    fn undo_after_a_removal_restores_what_it_took() {
        let editor = run(&canonical_script());
        let before = editor.document().clone();

        let (editor, outcome) = step(editor, "rm node review");
        assert!(
            outcome.text.contains("2 incident edge(s)"),
            "removing a node states how many edges went with it: {}",
            outcome.text
        );
        assert_eq!(editor.document().nodes.len(), 3);
        assert_eq!(editor.document().edges.len(), 1);

        let (editor, _) = step(editor, "undo");
        assert_eq!(
            editor.document(),
            &before,
            "undo restored the node and both of its edges"
        );
    }

    /// `read` is a recorded event carrying its document inline, so `undo`
    /// straight after one restores exactly the document that was there before.
    /// This is the answer to what undo means across a whole-document
    /// replacement: nothing special, because a read is not special.
    #[test]
    fn undo_after_read_restores_the_document_the_read_replaced() {
        let editor = run(&format!("add agent research --hash {HASH_A}\n"));
        let before = editor.document().clone();

        let incoming = include_str!("../../../examples/graphs/fold-refine.json");
        let (editor, outcome) = step(editor, &format!("read --json {}", compact_json(incoming)));
        assert_eq!(outcome.status, Status::Ok, "{}", outcome.text);
        assert!(
            outcome.text.contains("replacing the document of 1 node(s)"),
            "the read says what it replaced: {}",
            outcome.text
        );
        assert_eq!(editor.document().nodes.len(), 2);
        assert_ne!(editor.document(), &before);

        let (editor, _) = step(editor, "undo");
        assert_eq!(
            editor.document(),
            &before,
            "undo across a read restores the previous document exactly"
        );
        assert_eq!(editor.history().len(), 1, "only the original add remains");
    }

    /// Re-serializes a document to one line, so a `read --json` line in a test
    /// is a single token.
    fn compact_json(json: &str) -> String {
        let value: Value = serde_json::from_str(json).expect("parse");
        serde_json::to_string(&value).expect("serialize")
    }

    // --- the history round-trips --------------------------------------------

    /// Dumping the history, replaying it, and dumping again yields the identical
    /// document and the identical script. That is what makes a graph built by
    /// hand into something a repo can hold: the session IS the script.
    ///
    /// The document exercised here uses every construct that has to survive the
    /// trip: quoted names and prompts, JSON schemas, a tool's input mappings, a
    /// branch's two kinds of case, a map body, a fold's join rule, a labeled
    /// edge, a removal, and a whole-document read.
    #[test]
    fn the_command_history_round_trips() {
        let fold_document = compact_json(include_str!("../../../examples/graphs/fold-refine.json"));
        let editor = run(&format!(
            "read --json {fold_document}\n\
             add agent research --hash {HASH_A} --name \"Research the topic\" \
             --output-schema {{\"type\":\"object\",\"properties\":{{\"draft\":{{\"type\":\"string\"}}}}}}\n\
             add tool publish http_post --input body=approve.draft --input url=config.publish_url\n\
             add gate approve --approval-schema {{\"type\":\"object\"}} --prompt \"Approve the draft?\"\n\
             add branch route --on research.score --hash {HASH_B} --name \"Route on confidence\"\n\
             case route high --when \"score > 0.8\"\n\
             case route ask --model\n\
             add map fanout --over route.items --concurrency 4 --body research\n\
             add fold polish --body research --max-iterations 5 --stop-when \"score >= 0.9\" \
             --join best-by:review.overall_score\n\
             edge research route\n\
             edge route publish --label high\n\
             edge research approve\n\
             rm edge research approve\n"
        ));

        let script = editor.script();
        assert_eq!(
            script.lines().count(),
            editor.history().len(),
            "one line per recorded command"
        );

        let replayed = run(&script);
        assert_eq!(
            replayed.document(),
            editor.document(),
            "replaying the dumped script must rebuild the identical document\nscript:\n{script}"
        );
        assert_eq!(
            replayed.script(),
            script,
            "dumping the replay must reproduce the same script, character for character"
        );

        // Building the same history directly, with no lines involved, is the
        // same editor: the state really is a function of the command list.
        let rebuilt = Editor::from_history(editor.history().to_vec());
        assert_eq!(rebuilt, editor);
    }

    /// A dumped line is always in the resolved form, so a script replays with no
    /// filesystem: no `--file`, and a read carries its whole document inline.
    #[test]
    fn a_dumped_script_never_needs_a_host() {
        let fold_document = compact_json(include_str!("../../../examples/graphs/fold-refine.json"));
        let editor = run(&format!(
            "read --json {fold_document}\n\
             add agent research --hash {HASH_A}\n"
        ));
        let script = editor.script();
        assert!(
            !script.contains("--file"),
            "no line names a file:\n{script}"
        );
        for line in script.lines() {
            let parsed = parse(line)
                .expect("a dumped line parses")
                .expect("not blank");
            assert!(
                matches!(parsed, Line::Command(_)),
                "a dumped line must never ask the host: {line}"
            );
        }
    }

    // --- malformed lines ----------------------------------------------------

    /// A malformed line is a precise error naming the command and the token that
    /// broke it, never a panic and never a silently mangled command.
    #[test]
    fn a_malformed_line_is_a_precise_error() {
        let cases: &[(&str, ParseError)] = &[
            (
                "ad agent x --hash y",
                ParseError::UnknownCommand {
                    word: "ad".to_owned(),
                },
            ),
            (
                "add widget x",
                ParseError::UnknownNodeKind {
                    word: "widget".to_owned(),
                },
            ),
            (
                "add agent",
                ParseError::MissingArgument {
                    command: "add agent".to_owned(),
                    what: "a node id".to_owned(),
                },
            ),
            (
                "add agent research",
                ParseError::MissingOption {
                    command: "add agent".to_owned(),
                    option: "--hash or --file".to_owned(),
                },
            ),
            (
                "add agent research --hash",
                ParseError::MissingOptionValue {
                    command: "add agent".to_owned(),
                    option: "--hash".to_owned(),
                },
            ),
            (
                "add agent research --hash a --file b.toml",
                ParseError::ConflictingOptions {
                    command: "add agent".to_owned(),
                    first: "--hash".to_owned(),
                    second: "--file".to_owned(),
                },
            ),
            (
                "add agent research --hash a --colour red",
                ParseError::UnknownOption {
                    command: "add agent".to_owned(),
                    option: "--colour".to_owned(),
                },
            ),
            (
                "validate now",
                ParseError::UnexpectedArgument {
                    command: "validate".to_owned(),
                    found: "now".to_owned(),
                },
            ),
            (
                "add tool publish http_post --input bodyapprove.draft",
                ParseError::MalformedInputMapping {
                    found: "bodyapprove.draft".to_owned(),
                },
            ),
            (
                "add fold refine --body x --max-iterations 3 --stop-when s --join worst",
                ParseError::MalformedJoin {
                    found: "worst".to_owned(),
                },
            ),
            (
                "add map fanout --over items --concurrency lots --body x",
                ParseError::MalformedNumber {
                    command: "add map".to_owned(),
                    option: "--concurrency".to_owned(),
                    found: "lots".to_owned(),
                },
            ),
            ("add gate approve --approval-schema \"not json\"", {
                ParseError::MalformedJson {
                    command: "add gate".to_owned(),
                    option: "--approval-schema".to_owned(),
                    error: serde_json::from_str::<Value>("not json")
                        .expect_err("not json")
                        .to_string(),
                }
            }),
            (
                "add agent x --hash \"unclosed",
                ParseError::UnterminatedQuote,
            ),
            (
                "add gate approve --approval-schema {\"type\":\"object\"",
                ParseError::UnbalancedJson,
            ),
            (
                "rm",
                ParseError::MissingArgument {
                    command: "rm".to_owned(),
                    what: "node, edge, or case".to_owned(),
                },
            ),
            (
                "rm nodes research",
                ParseError::UnexpectedArgument {
                    command: "rm".to_owned(),
                    found: "nodes".to_owned(),
                },
            ),
            (
                "edge research",
                ParseError::MissingArgument {
                    command: "edge".to_owned(),
                    what: "a destination node id".to_owned(),
                },
            ),
            (
                "case route high",
                ParseError::MissingOption {
                    command: "case".to_owned(),
                    option: "--when or --model".to_owned(),
                },
            ),
        ];

        for (line, expected) in cases {
            let error = parse(line).expect_err(&format!("`{line}` must not parse"));
            assert_eq!(&error, expected, "wrong error for `{line}`");
            let message = error.to_string();
            assert!(
                !message.is_empty() && !message.contains("{"),
                "the message for `{line}` must be finished prose: {message}"
            );
        }
    }

    /// A blank line and a `#` comment are not commands and not errors, so an
    /// annotated script replays.
    #[test]
    fn blank_and_comment_lines_are_not_commands() {
        assert_eq!(parse("").expect("blank is fine"), None);
        assert_eq!(parse("   ").expect("whitespace is fine"), None);
        assert_eq!(parse("# a note").expect("a comment is fine"), None);
        assert_eq!(parse("   # indented").expect("still a comment"), None);
    }

    /// An inapplicable command is refused with a message that says which id was
    /// wrong, and the document is untouched.
    #[test]
    fn an_inapplicable_command_is_refused_without_changing_anything() {
        let editor = run(&format!("add agent research --hash {HASH_A}\n"));
        let before = editor.document().clone();

        for (line, expected) in [
            (
                format!("add agent research --hash {HASH_B}"),
                "already in the document",
            ),
            ("case research high --when x".to_owned(), "is a agent node"),
            ("case ghost high --when x".to_owned(), "no node `ghost`"),
            ("rm node ghost".to_owned(), "no node `ghost`"),
            ("rm edge research ghost".to_owned(), "no edge `research`"),
            ("rm case research high".to_owned(), "is a agent node"),
            ("show ghost".to_owned(), "no node `ghost`"),
            (
                "read --json {\"nodes\":[]}".to_owned(),
                "not a graph document",
            ),
            ("help nonsense".to_owned(), "no help for `nonsense`"),
        ] {
            let (next, outcome) = step(editor.clone(), &line);
            assert_eq!(
                outcome.status,
                Status::Refused,
                "`{line}` must be refused: {}",
                outcome.text
            );
            assert!(
                outcome.text.contains(expected),
                "`{line}` must be refused with {expected:?}, got: {}",
                outcome.text
            );
            assert_eq!(next.document(), &before, "`{line}` changed the document");
            assert_eq!(next.history().len(), 1, "`{line}` was recorded");
        }
    }

    // --- the host seam ------------------------------------------------------

    /// The three lines that name a host resource come back as requests rather
    /// than commands, and an agent file's draft completes into exactly the
    /// command a typed hash would have produced. This IS the seam the next
    /// phase resolves `--file research.toml` at.
    #[test]
    fn a_line_that_names_a_file_asks_the_host() {
        let parsed = parse("add agent research --file research.toml --name \"Research the topic\"")
            .expect("parses")
            .expect("not blank");
        let Line::Host(HostRequest::AgentFile { path, draft }) = parsed else {
            panic!("an agent file must come back as a host request, got {parsed:?}");
        };
        assert_eq!(path, "research.toml");
        assert_eq!(draft.node_id(), "research");
        assert_eq!(draft.node_kind(), "agent");

        // The host resolves the file to a hash and completes the draft. The
        // result is indistinguishable from the typed-hash form.
        let resolved = draft.with_hash(HASH_A);
        let typed = parse(&format!(
            "add agent research --hash {HASH_A} --name \"Research the topic\""
        ))
        .expect("parses")
        .expect("not blank");
        assert_eq!(Line::Command(resolved), typed);

        // The same seam serves a branch's model-decision agent.
        let parsed = parse("add branch route --file judge.toml")
            .expect("parses")
            .expect("not blank");
        let Line::Host(HostRequest::AgentFile { path, draft }) = parsed else {
            panic!("a branch file must come back as a host request");
        };
        assert_eq!(path, "judge.toml");
        assert_eq!(draft.node_kind(), "branch");
        let Command::Add(node) = draft.with_hash(HASH_B) else {
            panic!("a completed draft is an add");
        };
        let Node::Branch(branch) = *node else {
            panic!("of a branch node");
        };
        assert_eq!(branch.agent_hash.as_deref(), Some(HASH_B));

        // Reading and writing a path are the other two halves the host owns.
        assert_eq!(
            parse("read graph.json").expect("parses"),
            Some(Line::Host(HostRequest::ReadFile {
                path: "graph.json".to_owned()
            }))
        );
        assert_eq!(
            parse("write graph.json").expect("parses"),
            Some(Line::Host(HostRequest::WriteFile {
                path: "graph.json".to_owned()
            }))
        );
        // Bare `write` needs no host: the JSON comes back to the caller.
        assert_eq!(
            parse("write").expect("parses"),
            Some(Line::Command(Command::Write))
        );
    }

    /// `write` hands back JSON that parses straight back to the same document,
    /// ends in a newline so it is a well-formed file, and does not require the
    /// document to be valid first.
    #[test]
    fn write_returns_json_the_document_parses_back_from() {
        let editor = run(&canonical_script());
        let (editor, outcome) = step(editor, "write");
        assert_eq!(outcome.status, Status::Ok);
        let json = outcome.document_json.expect("write returns the document");
        assert!(json.ends_with('\n'), "a written file ends in a newline");
        let parsed: Graph = serde_json::from_str(&json).expect("the written JSON parses");
        assert_eq!(&parsed, editor.document());

        // An unfinished document is writable too, so work in progress can be
        // saved and picked up later.
        let (_, outcome) = step(run("edge nowhere nohow\n"), "write");
        assert_eq!(outcome.status, Status::Ok);
        assert!(outcome.document_json.is_some());
    }

    // --- what the reader sees -----------------------------------------------

    /// `show` reads in a narrow pane: only line breaks move between widths, and
    /// no line of the outline exceeds the width it was rendered at.
    #[test]
    fn show_wraps_to_a_narrow_pane() {
        let editor = run(&format!(
            "add agent research --hash {HASH_A} --name \"Research the topic thoroughly\" \
             --output-schema {{\"type\":\"object\"}}\n\
             add tool publish http_post --input body=approve.draft --input url=config.publish_url \
             --input retries=config.retry_count\n\
             add gate approve --approval-schema {{\"type\":\"object\"}} \
             --prompt \"Approve this draft for publication?\"\n\
             edge research publish\n\
             edge publish approve\n"
        ));

        let words =
            |text: &str| -> Vec<String> { text.split_whitespace().map(str::to_owned).collect() };
        let (_, narrow) = step(editor.clone(), "show");
        let (_, wide) = step(editor.clone(), "show");
        let narrow_text = editor.show(None, 40).expect("outline");
        let wide_text = editor.show(None, 100).expect("outline");
        assert_eq!(
            words(&narrow_text),
            words(&wide_text),
            "wrapping may only move line breaks, never words"
        );
        assert_eq!(words(&narrow.text), words(&wide.text));

        for line in narrow_text.lines() {
            assert!(
                line.len() <= 40,
                "the outline must not exceed its width: {line:?}\n{narrow_text}"
            );
        }

        // The outline is an outline, not JSON: a long hash is shortened and a
        // schema is named by its type rather than printed.
        assert!(
            narrow_text.contains("sha256:1111111\u{2026}"),
            "the outline shortens a hash:\n{narrow_text}"
        );
        assert!(
            narrow_text.contains("output_schema object"),
            "the outline names a schema by its type:\n{narrow_text}"
        );
        assert!(
            !narrow_text.contains('{'),
            "no JSON in the outline:\n{narrow_text}"
        );
    }

    /// `show <ID>` is where the full hash and the whole schema live, and unlike
    /// the outline its aligned block and pretty JSON are deliberately not
    /// reflowed: a reader checks them against a file.
    #[test]
    fn show_one_node_prints_it_in_full() {
        let editor = run(&format!(
            "add agent research --hash {HASH_A} --name \"Research the topic\" \
             --output-schema {{\"type\":\"object\",\"properties\":{{\"draft\":{{\"type\":\"string\"}}}}}}\n"
        ));
        let (_, outcome) = step(editor, "show research");
        assert_eq!(outcome.status, Status::Ok);
        assert!(
            outcome.text.contains(HASH_A),
            "the whole hash, not a short one:\n{}",
            outcome.text
        );
        assert!(
            outcome.text.contains("\"draft\""),
            "the whole schema, pretty-printed:\n{}",
            outcome.text
        );
        assert!(outcome.text.starts_with("agent node `research`\n"));
    }

    /// An empty document says so, and points at `help` rather than printing an
    /// empty outline nobody can act on.
    #[test]
    fn an_empty_document_says_where_to_start() {
        let (_, outcome) = step(Editor::new(), "show");
        assert_eq!(outcome.status, Status::Ok);
        assert!(outcome.text.contains("graph: empty"));
        assert!(outcome.text.contains("help"));

        let (_, outcome) = step(Editor::new(), "history");
        assert!(outcome.text.contains("No commands recorded yet"));
    }

    /// Every command in the help table has help, every command the parser
    /// accepts is in the table, and the grammar forms are never wrapped, since
    /// a form broken across lines is a form you cannot type.
    #[test]
    fn help_covers_every_command_and_never_breaks_a_form() {
        let overview = help(None, WIDTH).expect("the overview");
        for word in COMMAND_WORDS.split(", ") {
            assert!(
                overview.contains(word),
                "the overview lists {word}:\n{overview}"
            );
            let topic = help(Some(word), 40).expect("every listed command has help");
            let entry = TOPICS
                .iter()
                .find(|entry| entry.name == word)
                .expect("in the table");
            for form in entry.forms {
                assert!(
                    topic.lines().any(|line| line.trim() == *form),
                    "the form `{form}` must survive on one line:\n{topic}"
                );
            }
        }
        assert_eq!(TOPICS.len(), COMMAND_WORDS.split(", ").count());
    }

    /// `rm edge` with no label removes every edge between the two nodes; with a
    /// label, only the edges carrying it.
    #[test]
    fn removing_an_edge_narrows_by_label_only_when_asked() {
        let script = "add branch route --on x\n\
                      add agent worker --hash h\n\
                      edge route worker --label high\n\
                      edge route worker --label low\n\
                      edge route worker\n";

        let editor = run(script);
        let (editor, outcome) = step(editor, "rm edge route worker --label high");
        assert!(
            outcome.text.contains("removed 1 edge(s)"),
            "{}",
            outcome.text
        );
        assert_eq!(editor.document().edges.len(), 2);

        let editor = run(script);
        let (editor, outcome) = step(editor, "rm edge route worker");
        assert!(
            outcome.text.contains("removed 3 edge(s)"),
            "{}",
            outcome.text
        );
        assert!(editor.document().edges.is_empty());
    }

    /// A read brings in a document whatever its state: one that does not
    /// validate reads fine, and only one that does not PARSE is refused, with
    /// the strict parser's message naming the offending field.
    #[test]
    fn read_accepts_an_invalid_document_but_not_an_unparseable_one() {
        let (editor, outcome) = step(
            Editor::new(),
            "read --json {\"schema_version\":1,\"nodes\":[{\"kind\":\"agent\",\"payload\":\
             {\"id\":\"a\",\"agent_hash\":\"nope\"}}],\"edges\":[{\"from\":\"a\",\"to\":\"ghost\"}]}",
        );
        assert_eq!(outcome.status, Status::Ok, "{}", outcome.text);
        assert_eq!(editor.document().nodes.len(), 1);
        let (_, outcome) = step(editor, "validate");
        assert_eq!(outcome.status, Status::Invalid);

        let (_, outcome) = step(
            Editor::new(),
            "read --json {\"schema_version\":1,\"nodes\":[],\"surprise\":true}",
        );
        assert_eq!(outcome.status, Status::Refused);
        assert!(
            outcome.text.contains("surprise"),
            "the strict parser names the stray field: {}",
            outcome.text
        );
    }

    /// A whole session, rendered as the transcript a reader would see. This
    /// pins the composed output rather than one function's, which is what
    /// catches a message that reads badly next to its neighbours, and it is the
    /// test to run with `--nocapture` to look at the editor.
    #[test]
    fn a_whole_session_reads_as_a_transcript() {
        let script = format!(
            "add agent research --hash {HASH_A} --name \"Research the topic\" \
             --output-schema {{\"type\":\"object\"}}\n\
             add gate approve --approval-schema {{\"type\":\"object\"}} \
             --prompt \"Approve this draft?\"\n\
             edge research approve\n\
             edge approve publish\n\
             validate\n\
             add tool publish http_post --input body=approve.draft\n\
             validate\n\
             show\n\
             undo\n\
             history\n"
        );

        let mut editor = Editor::new();
        let mut transcript = String::new();
        // The output lines only, kept apart from the echoed input so the width
        // check below measures what the editor WROTE.
        let mut written: Vec<String> = Vec::new();
        for line in script.lines() {
            let Some(Line::Command(command)) = parse(line).expect("a well-formed line") else {
                continue;
            };
            let (next, outcome) = editor.apply(command, WIDTH);
            transcript.push_str(&format!("> {line}\n{}", outcome.text));
            written.extend(outcome.text.lines().map(str::to_owned));
            editor = next;
        }
        println!("{transcript}");

        // The transcript tells one coherent story: an edge ahead of its target
        // is accepted with a note, validate names the dangling endpoint, adding
        // the node clears it, and undo names the exact line it dropped.
        for expected in [
            "added agent node `research`",
            "no node `publish` in the document yet",
            "graph invalid: 1 problem(s)",
            "unknown node id `publish`",
            "graph ok: 3 node(s), 2 edge(s)",
            "undid `add tool publish http_post --input body=approve.draft`",
        ] {
            assert!(
                transcript.contains(expected),
                "the transcript must contain {expected:?}:\n{transcript}"
            );
        }
        // Nothing the editor wrote overflows the pane, except the one category
        // this crate deliberately never wraps: a command line a reader copies.
        // The `history` dump is exactly that, so a dumped line is exempt for
        // the same reason `render`'s resume commands are.
        for line in &written {
            let is_command = matches!(parse(line), Ok(Some(_)));
            assert!(
                line.len() <= WIDTH || is_command,
                "the editor wrote a line over width {WIDTH} that is not a command: {line:?}"
            );
        }
    }
}
