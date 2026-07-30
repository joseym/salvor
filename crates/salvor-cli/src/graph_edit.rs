//! `salvor graph edit`: the host half of the graph editor.
//!
//! [`salvor_cli_core::graph_editor`] is the editor itself, and it performs no
//! IO: one typed line is one command, the command list is the state, and the
//! document is a pure function of that list. This module is the terminal and
//! the filesystem around it. Read a line, hand it to the editor's own
//! [`parse`], apply what comes back, print what that produced.
//!
//! # Three requests, and nothing else
//!
//! A line that names a FILE cannot become a command inside a crate with no
//! filesystem, so [`parse`] returns a [`HostRequest`] for those three lines and
//! this module serves them:
//!
//! - `read <PATH>` reads the file and applies [`Command::Read`] with its text.
//! - `write <PATH>` applies [`Command::Write`] and writes back the JSON that
//!   comes with the outcome.
//! - an agent node's `--file <PATH>` is resolved to a `sha256:` definition hash
//!   by [`resolve_agent_hash`], and [`HashDraft::with_hash`] turns the draft
//!   into the identical command the typed-hash form produces.
//!
//! Nothing else here decides what a line means. There is no second parser, no
//! second validator, and no command this module understands that the editor
//! does not, which is what keeps a session in a browser terminal (with no
//! filesystem, so it refuses those three lines instead) the same editor as this
//! one.
//!
//! # Which stream carries what
//!
//! Stdout carries what the editor produced: every outcome it accepted, the
//! document a bare `write` hands back, the grammar `help` prints. Stderr
//! carries the conversation around it: the banner, the prompt, and every line
//! the editor refused. That is the same split the rest of the CLI draws
//! (`salvor list`'s table on stdout, its progress on stderr), and it means a
//! redirected session's stdout is the transcript alone while the failures are
//! still on screen.
//!
//! # Line editing, and where Tab's answers come from
//!
//! A terminal session reads through a line editor, so an author gets arrow
//! keys, a history of the lines they typed, and Tab. What Tab OFFERS is not
//! decided here: [`Editor::candidates`] answers it, from the document being
//! built and from the same table of forms `help` prints. That split is forced
//! rather than chosen. The interesting candidates are node ids and branch case
//! names, which only the editor's own state knows, and raw mode is IO, which
//! the editor's crate cannot have and still compile for the browser. So the
//! candidates come from there and the keypress is handled here, and there is no
//! second list of commands or options anywhere for the grammar to drift from.
//!
//! The one candidate the editor cannot produce is a FILE, and it says so rather
//! than guessing: a position its forms give a `<PATH>` comes back as
//! [`Candidates::Path`], and all this module does with that is list the
//! directory, through the line editor's own filename completer. So `read`,
//! `write`, and a node's `--file` complete to real files while WHICH positions
//! those are stays a fact about the grammar, in the crate that holds it. This
//! module could not offer a path in the wrong place if it tried, which is the
//! same guarantee the three [`HostRequest`]s give: the host serves a file, it
//! never decides where one goes.
//!
//! Completion is a terminal's alone. Off a terminal this loop is the plain read
//! it always was, down to the first-failure-ends-the-session rule, because a
//! script has no cursor to complete at.
//!
//! # How a session ends
//!
//! At end of input, and only there: `Ctrl-D` at a terminal, the last line of a
//! redirected script. There is deliberately no `quit` command, because the
//! grammar belongs to the editor and the editor has no loop to leave; a word
//! this module intercepted would be a word that works here and nowhere else.
//!
//! `Ctrl-C` at a terminal throws away the line being typed and prompts again,
//! which is the only thing it CAN mean once there is a line to throw away. A
//! session holds a document that has not been written yet, so the reading that
//! ends it on a keystroke people press to mean "not that line" is the reading
//! that loses work.
//!
//! The exit code is 0 unless a line was refused, did not parse, or named a file
//! the host could not use. A `validate` that reports an invalid document is NOT
//! one of those: asking a broken document to validate is a question answered,
//! not a command refused, and a partial document is a legal document while it
//! is being built. `salvor graph validate` is the verb that gates on a
//! document's validity.
//!
//! When input is not a terminal, the first failure ends the session. A script
//! is a program, and carrying on past a line that did not take would build a
//! document nobody described and then write it out as though it were the one
//! asked for. A person at a prompt is right there to retype the line, so an
//! interactive session carries on.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::config::{Behavior, BellStyle, CompletionType, Config};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::MemHistory;
use rustyline::validate::Validator;
// `Context` is spelled out at its one use below: the name is already taken here
// by anyhow's trait, which every host-side failure in this module goes through.
use rustyline::Helper;
use salvor_cli_core::graph_editor::{
    Candidates, Command, Editor, HashDraft, HostRequest, Line, Outcome, Status, parse,
};

use crate::cli::GraphEditArgs;
use crate::render::DEFAULT_REPORT_WIDTH;

/// The prompt an interactive session shows, on stderr.
const PROMPT: &str = "graph> ";

/// What a person is told before the first prompt: where the grammar is, that a
/// document nobody wrote to a file is a document nobody keeps, and how to
/// leave.
///
/// Written as short fixed lines rather than reflowed to the pane, because it is
/// the only text this loop prints that is not the editor's own: every line fits
/// inside [`MIN_PANE_WIDTH`], so there is no width to consult.
const BANNER: &str = "A graph document, one line at a time.\n\
                      `help` lists the commands; Tab completes.\n\
                      `write <PATH>` saves; Ctrl-D leaves.";

/// The narrowest pane the editor's prose is wrapped to.
///
/// A real terminal narrower than this is honoured up to a point and no further:
/// prose reflowed to twenty columns is a column of single words, which is less
/// readable than a line that overruns.
const MIN_PANE_WIDTH: usize = 40;

/// `salvor graph edit`: read editor commands and fold them into a document.
///
/// Returns `Ok(0)` for a session in which every line took and `Ok(1)` when one
/// did not, the positional FILE failing to open among them: that failure comes
/// back through the editor's own request path, so it reads exactly like the
/// refused `read <PATH>` line it is. `Err(..)` is for the two things that are
/// not a line at all: an unreadable `--script`, whose value could not be used
/// rather than a command that did not take, and input that is not text.
pub async fn edit(args: GraphEditArgs) -> Result<u8> {
    // The prompt and the banner are for a person, and the person is whoever is
    // typing: stdin is the stream that says whether there is one.
    let interactive = std::io::stdin().is_terminal();
    let mut session = Session::new(pane_width());

    if let Some(path) = args.path.as_deref() {
        session.open(path).await;
        if session.refused {
            // The named document is what the session is about, so failing to
            // open it ends the session rather than starting one line down. An
            // editor that opened nothing is an editor holding an empty
            // document, and an empty document is exactly what a later `write`
            // would put over the file that was named.
            return Ok(1);
        }
    }
    if let Some(path) = args.script.as_deref() {
        let script = std::fs::read_to_string(path)
            .with_context(|| format!("reading the command script {}", path.display()))?;
        for line in script.lines() {
            session.feed(line).await;
            if session.refused {
                // A script's first bad line ends it whoever is driving: the
                // lines after it were written expecting the one before them to
                // have taken.
                return Ok(1);
            }
        }
    }

    if interactive {
        eprintln!("{BANNER}");
        match line_editor() {
            Some(reader) => read_with_line_editing(&mut session, reader).await?,
            // A terminal the line editor will not open on (no controlling
            // terminal, a TERM it refuses) is still a terminal someone is
            // typing at, so the session falls back to the plain read rather
            // than to no session at all. They lose Tab, not their document.
            None => read_a_line_at_a_time(&mut session, true).await?,
        }
    } else {
        read_a_line_at_a_time(&mut session, false).await?;
    }

    Ok(u8::from(session.refused))
}

/// Reads the session's lines one at a time, with no editing of any kind.
///
/// This is the whole loop off a terminal, and the fallback on one: a plain
/// `read_line`, a prompt only when someone is there to read it, and a script's
/// first failure ending it, because the lines after a failed one were written
/// expecting it to have taken.
async fn read_a_line_at_a_time(session: &mut Session, interactive: bool) -> Result<()> {
    // One locked handle for the session, so a person and a redirected script
    // drive the identical loop a line at a time.
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut buffer = String::new();
    loop {
        if interactive {
            eprint!("{PROMPT}");
            // Rust's stderr is unbuffered, so this is belt and braces rather
            // than load-bearing; a prompt that does not appear before the read
            // blocks is the one bug worth being sure of.
            let _ = std::io::stderr().flush();
        }
        buffer.clear();
        // Zero bytes is end of input: Ctrl-D at a terminal, or a script that
        // ran out of lines. Either way the session is over.
        let read = input
            .read_line(&mut buffer)
            .context("reading a line of editor input")?;
        if read == 0 {
            if interactive {
                // The prompt was printed and nothing followed it, so close the
                // line the shell would otherwise write its own prompt onto.
                eprintln!();
            }
            break;
        }
        session.feed(buffer.trim_end_matches(['\n', '\r'])).await;
        if session.refused && !interactive {
            break;
        }
    }

    Ok(())
}

/// Reads the session's lines through the line editor, so Tab completes and the
/// arrow keys walk back through what was typed.
///
/// A refusal does not end this loop, exactly as it did not before: the person
/// who typed the line is right there to retype it.
async fn read_with_line_editing(session: &mut Session, mut reader: LineReader) -> Result<()> {
    loop {
        // Tab is answered from the document as it stands, so the completer's
        // copy of the editor is refreshed before every read. A document is a
        // handful of nodes, and this is once per typed line, so the clone costs
        // nothing worth avoiding to share one.
        if let Some(grammar) = reader.helper_mut() {
            grammar.editor = session.editor.clone();
        }
        match reader.readline(PROMPT) {
            Ok(line) => {
                // Only a line with something on it is worth recalling. A
                // failure to record one is not worth mentioning to anyone: the
                // line still runs.
                if !line.trim().is_empty() {
                    let _ = reader.add_history_entry(line.as_str());
                }
                session.feed(line.trim_end_matches(['\n', '\r'])).await;
            }
            // Ctrl-D on an empty line: end of input, exactly as it is for a
            // script that ran out of lines. The newline closes the line the
            // prompt is sitting on.
            Err(ReadlineError::Eof) => {
                eprintln!();
                break;
            }
            // Ctrl-C: the line being typed is abandoned and the next one asked
            // for. See the module docs for why this does not end the session.
            Err(ReadlineError::Interrupted) => eprintln!(),
            Err(error) => {
                return Err(anyhow::Error::new(error).context("reading a line of editor input"));
            }
        }
    }

    Ok(())
}

/// The line editor for a terminal session, or `None` when one cannot be opened.
///
/// Three settings, each of them load-bearing:
///
/// - [`Behavior::PreferTerm`] draws the prompt and the line being edited on
///   `/dev/tty` rather than on stdout, which is what keeps this module's stream
///   split intact: stdout stays the transcript alone even while someone is
///   typing at it, so `salvor graph edit > flow.log` records what the editor
///   produced and nothing of the conversation around it.
/// - [`CompletionType::List`] means one candidate is inserted on the first Tab
///   and several insert as much as they agree on, with the list printed on a
///   second Tab. The alternative, `Circular`, would offer the six node kinds one
///   keypress at a time.
/// - [`BellStyle::None`] because a position with nothing to offer must do
///   nothing, and the library's answer to no candidates is otherwise to beep.
///
/// The history is [`MemHistory`]: it lasts the session and is never written
/// anywhere. A file would mean choosing a directory in someone's home and
/// leaving a document's worth of ids in it, which is not a thing to start doing
/// as a side effect of Tab working.
fn line_editor() -> Option<LineReader> {
    let config = Config::builder()
        .behavior(Behavior::PreferTerm)
        .completion_type(CompletionType::List)
        .bell_style(BellStyle::None)
        .build();
    let mut reader = LineReader::with_history(config, MemHistory::new()).ok()?;
    reader.set_helper(Some(Grammar {
        editor: Editor::new(),
        paths: FilenameCompleter::new(),
    }));
    Some(reader)
}

/// The line editor, with the completer this module gives it.
type LineReader = rustyline::Editor<Grammar, MemHistory>;

/// What answers Tab: a copy of the editor as the session last left it.
///
/// The state is the whole point. `add`, `--join`, and `--label` could be
/// completed from a fixed list, but a node id and a branch case name could not,
/// and those are the words an author actually has to get right.
struct Grammar {
    /// The document being built, refreshed before every read.
    editor: Editor,
    /// The line editor's own filename completer, for the positions the editor
    /// reports as paths. Listing a directory is all it is asked for: the
    /// question of WHERE a path belongs is answered in the editor's crate,
    /// beside the forms that say so.
    paths: FilenameCompleter,
}

impl Completer for Grammar {
    // A pair rather than a bare word because a path is listed and inserted
    // differently: the list shows the entry's own name while the line takes the
    // whole path leading to it. A word candidate is both at once.
    type Candidate = Pair;

    /// Asks the editor what can go in the place of the word the cursor is in.
    ///
    /// Only the text LEFT of the cursor is offered up, and the replacement spans
    /// from the start of that word to the cursor, so completing in the middle of
    /// a line leaves the rest of it alone.
    ///
    /// The word starts after the last whitespace before the cursor. That is the
    /// same boundary [`Editor::candidates`] filters on, and the two cannot
    /// disagree where it matters: a word that contains whitespace is a quoted
    /// string or a JSON argument, and the editor offers nothing for either, so
    /// the start is never used.
    ///
    /// A path is spanned by the filename completer instead, from its own reading
    /// of where the path being typed starts: a path may hold characters a word
    /// may not, and the span has to match the candidates it comes back with.
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _context: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let partial = &line[..pos];
        let words = match self.editor.candidates(partial) {
            // The editor says a file goes here, so the directory is listed and
            // nothing else is consulted. A directory comes back with its
            // trailing separator, which is what lets the next Tab carry on
            // INTO it rather than stopping at its name.
            Candidates::Path => return self.paths.complete_path(line, pos),
            Candidates::Words(words) => words,
        };
        if words.is_empty() {
            // Nothing to offer, so nothing is replaced and nothing is said.
            return Ok((pos, Vec::new()));
        }
        // A word is inserted exactly as it is listed.
        let candidates = words
            .into_iter()
            .map(|word| Pair {
                display: word.clone(),
                replacement: word,
            })
            .collect();
        Ok((word_start(partial), candidates))
    }
}

/// The byte offset the word at the end of `partial` starts at.
fn word_start(partial: &str) -> usize {
    match partial
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
    {
        Some((at, c)) => at + c.len_utf8(),
        None => 0,
    }
}

// The rest of what the line editor asks of a helper, none of which this loop
// wants: no inline hint, no syntax colouring, and no line the editor would
// refuse to hand over, since judging a line is `parse`'s job and a refusal is
// something the session prints rather than something the prompt withholds.
impl Hinter for Grammar {
    type Hint = String;
}
impl Highlighter for Grammar {}
impl Validator for Grammar {}
impl Helper for Grammar {}

/// One editing session: the editor, the pane it prints into, and whether
/// anything in it was refused.
struct Session {
    /// The editor. Moved through every [`Editor::apply`] call, since the fold's
    /// shape is `(state, command) -> (state, output)`.
    editor: Editor,
    /// The column count prose wraps to.
    width: usize,
    /// True once a line was refused, did not parse, or named a file the host
    /// could not use. It only ever goes from false to true: the exit code says
    /// that something in this session did not take, whatever came after it.
    refused: bool,
}

impl Session {
    /// A session over an empty document.
    fn new(width: usize) -> Self {
        Self {
            editor: Editor::new(),
            width,
            refused: false,
        }
    }

    /// Opens a document as the session's starting point.
    ///
    /// Served through the same [`HostRequest::ReadFile`] a typed `read <PATH>`
    /// produces, so what the argument does and what the line does are one code
    /// path, down to the message a file that is not a document earns. The
    /// caller reads [`Self::refused`] to find out whether it worked.
    async fn open(&mut self, path: &Path) {
        self.serve(HostRequest::ReadFile {
            path: path.display().to_string(),
        })
        .await;
    }

    /// Reads one line and does what it says.
    async fn feed(&mut self, line: &str) {
        let parsed = match parse(line) {
            Ok(Some(parsed)) => parsed,
            // A blank line or a `#` comment is not a command and not a
            // mistake, which is what lets a dumped script carry annotations.
            Ok(None) => return,
            Err(error) => return self.refuse(&error.to_string()),
        };
        match parsed {
            Line::Command(command) => self.apply(command),
            Line::Host(request) => self.serve(request).await,
        }
    }

    /// Serves the one thing a line needed from the host, then applies the
    /// command that came of it.
    async fn serve(&mut self, request: HostRequest) {
        match request {
            HostRequest::AgentFile { path, draft } => self.resolve(&path, draft).await,
            HostRequest::ReadFile { path } => match std::fs::read_to_string(&path) {
                Ok(json) => self.apply(Command::Read { json }),
                Err(error) => self.refuse(&format!("cannot read {path}: {error}")),
            },
            HostRequest::WriteFile { path } => {
                let outcome = self.fold(Command::Write);
                // `Command::Write` always carries the document; the editor's
                // own type says so, and a missing one would be a bug in it
                // rather than anything an author could type.
                let Some(json) = outcome.document_json.as_deref() else {
                    self.refuse("the editor serialized no document to write");
                    return;
                };
                match std::fs::write(&path, json) {
                    Ok(()) => {
                        self.show(&outcome);
                        println!("wrote {path}");
                    }
                    Err(error) => self.refuse(&format!("cannot write {path}: {error}")),
                }
            }
        }
    }

    /// Resolves an agent node's `--file` to a definition hash and adds the node.
    async fn resolve(&mut self, path: &str, draft: HashDraft) {
        // Said before it happens, because building a definition connects to
        // every MCP server it declares: this is the one editor line that can
        // take a noticeable moment, and silence in a prompt loop reads as a
        // hang.
        eprintln!(
            "resolving {} node `{}` from {path}",
            draft.node_kind(),
            draft.node_id()
        );
        match resolve_agent_hash(Path::new(path)).await {
            Ok(hash) => {
                // The line named a path and the document records a hash, so the
                // substitution is printed rather than left to be discovered by
                // a later `show`.
                println!("{path} is {hash}");
                self.apply(draft.with_hash(hash));
            }
            Err(error) => self.refuse(&format!("cannot resolve {path}: {error:#}")),
        }
    }

    /// Applies a command and shows what it produced.
    fn apply(&mut self, command: Command) {
        let outcome = self.fold(command);
        self.show(&outcome);
        if let Some(json) = &outcome.document_json {
            // A bare `write` names no destination, so the document goes to the
            // stream this session is already writing to.
            print!("{json}");
        }
    }

    /// Applies one command to the editor and hands back the outcome, printing
    /// nothing. The editor is swapped out and back because the fold takes it by
    /// value.
    fn fold(&mut self, command: Command) -> Outcome {
        let (editor, outcome) = std::mem::take(&mut self.editor).apply(command, self.width);
        self.editor = editor;
        outcome
    }

    /// Prints an outcome on the stream its status calls for.
    fn show(&mut self, outcome: &Outcome) {
        match outcome.status {
            Status::Refused => self.refuse(outcome.text.trim_end()),
            // An invalid document is an answer to `validate`, not a failed
            // command, so it prints as output and leaves the exit code alone.
            Status::Ok | Status::Invalid => print!("{}", outcome.text),
        }
    }

    /// Reports something that did not take, and remembers that it did not.
    fn refuse(&mut self, message: &str) {
        self.refused = true;
        eprintln!("{message}");
    }
}

/// Resolves an agent definition file to the `sha256:` hash a graph `agent` node
/// references, by exactly the path `salvor agent hash` takes.
///
/// The hash covers the BUILT definition, tool schemas included, and an MCP
/// server is the only thing that can state its own tools, so no hash of the
/// file's bytes could be the number a graph node has to match. The definition
/// is therefore built through the same [`crate::commands::build_agents`] every
/// verb taking `--agent` uses, which connects to whatever servers it declares.
///
/// Those sessions are closed before the hash is returned. An editor waits at a
/// prompt for as long as the author is thinking, and a subprocess left running
/// for a line that has already finished is a subprocess nobody will remember to
/// stop.
async fn resolve_agent_hash(path: &Path) -> Result<String> {
    let paths = [PathBuf::from(path)];
    let (agents, servers) = crate::commands::build_agents(&paths).await?;
    crate::commands::close_servers(servers).await;
    let agent = agents
        .first()
        .context("building the definition produced no agent")?;
    Ok(agent.def_hash().to_owned())
}

/// The column count the editor's prose wraps to: the real terminal's width when
/// there is one, else [`DEFAULT_REPORT_WIDTH`].
///
/// Measured on the stream the wrapped text goes to. A session whose output is
/// piped or captured has no width to honour, so it takes the same default every
/// other report in this CLI wraps to, which is what makes a recorded or tested
/// session's output stable.
fn pane_width() -> usize {
    pane(terminal_size::terminal_size_of(std::io::stdout()).map(|(width, _)| width.0))
}

/// Folds a measured column count into the width to wrap to. Split out from
/// [`pane_width`] because the measurement needs a terminal and this decision
/// does not.
fn pane(columns: Option<u16>) -> usize {
    match columns {
        Some(columns) => usize::from(columns).max(MIN_PANE_WIDTH),
        None => DEFAULT_REPORT_WIDTH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No terminal means no width to honour, so the session wraps to the same
    /// number every other report in this CLI does. That is what makes a piped
    /// or captured session's output stable, which the integration tests and a
    /// recorded session both rest on.
    #[test]
    fn a_pane_with_no_terminal_takes_the_default_width() {
        assert_eq!(pane(None), DEFAULT_REPORT_WIDTH);
    }

    /// A real terminal's width is used as it stands, wider than the default
    /// included: the point of measuring is to fill the pane the author has.
    #[test]
    fn a_real_terminal_width_is_honoured() {
        assert_eq!(pane(Some(132)), 132);
        assert_eq!(pane(Some(64)), 64);
    }

    /// Below the floor the floor wins, because prose reflowed into a column of
    /// single words is harder to read than a line that overruns.
    #[test]
    fn an_absurdly_narrow_terminal_is_floored() {
        assert_eq!(pane(Some(12)), MIN_PANE_WIDTH);
        assert_eq!(pane(Some(0)), MIN_PANE_WIDTH);
    }

    /// The word a completion replaces starts after the last whitespace before
    /// the cursor, and the offset is in BYTES because that is what the line
    /// editor splices on.
    #[test]
    fn a_completion_replaces_the_word_the_cursor_is_in() {
        assert_eq!(word_start(""), 0);
        assert_eq!(word_start("ad"), 0);
        // At a boundary the word is empty, so it starts where the cursor is and
        // the candidate is inserted rather than replacing anything.
        assert_eq!(word_start("add "), 4);
        assert_eq!(word_start("add ag"), 4);
        assert_eq!(word_start("  edge  res"), 8);
        // A multi-byte character before the word does not shift the offset onto
        // a byte that is not a character boundary.
        assert_eq!(word_start("show \u{e9}tape"), 5);
        assert_eq!(word_start("show \u{e9}tape ne"), 12);
    }

    /// A completer over a two-node document, for the tests below.
    fn grammar() -> Grammar {
        let hash = format!("sha256:{}", "1".repeat(64));
        let mut editor = Editor::new();
        for line in [
            format!("add agent research --hash {hash}"),
            "add gate approve --approval-schema {\"type\":\"object\"}".to_owned(),
        ] {
            let Ok(Some(Line::Command(command))) = parse(&line) else {
                panic!("`{line}` is a command");
            };
            let (next, _) = editor.apply(command, DEFAULT_REPORT_WIDTH);
            editor = next;
        }
        Grammar {
            editor,
            paths: FilenameCompleter::new(),
        }
    }

    /// What each candidate would put on the line.
    fn inserted(candidates: &[Pair]) -> Vec<&str> {
        candidates
            .iter()
            .map(|pair| pair.replacement.as_str())
            .collect()
    }

    /// The seam, driven the way the line editor drives it: a document, a partial
    /// line, and the span plus candidates that come back. The completer holds a
    /// copy of the editor, so the ids it offers are the ids that document has.
    #[test]
    fn the_completer_answers_from_the_document_it_holds() {
        let grammar = grammar();
        let history = MemHistory::new();
        let context = rustyline::Context::new(&history);

        // Mid-word: the span starts at the word, so the candidate replaces it.
        let line = "edge rese";
        let (start, offered) = grammar
            .complete(line, line.len(), &context)
            .expect("completing never fails");
        assert_eq!(&line[start..], "rese");
        assert_eq!(inserted(&offered), ["research"]);

        // At a boundary: the span is empty and every id is offered.
        let line = "edge research ";
        let (start, offered) = grammar
            .complete(line, line.len(), &context)
            .expect("completing never fails");
        assert_eq!(start, line.len());
        assert_eq!(inserted(&offered), ["research", "approve"]);

        // Only the text left of the cursor is read, so completing in the middle
        // of a line leaves the rest of it alone.
        let line = "edge rese route";
        let (start, offered) = grammar
            .complete(line, 9, &context)
            .expect("completing never fails");
        assert_eq!(&line[start..9], "rese");
        assert_eq!(inserted(&offered), ["research"]);

        // Nothing to offer replaces nothing and says nothing.
        let line = "add agent ";
        let (start, offered) = grammar
            .complete(line, line.len(), &context)
            .expect("completing never fails");
        assert_eq!(start, line.len());
        assert!(offered.is_empty());
    }

    /// The other half of the seam: where the editor reports a path, real
    /// directory entries come back. The editor named the position and this
    /// module listed the directory, which is the whole of the split.
    #[test]
    fn a_path_position_completes_to_what_the_directory_holds() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("flow.json"), "{}").expect("write");
        std::fs::create_dir(dir.path().join("drafts")).expect("mkdir");
        let here = dir.path().display().to_string();

        let grammar = grammar();
        let history = MemHistory::new();
        let context = rustyline::Context::new(&history);

        // Every entry, at the boundary after `read`.
        let line = format!("read {here}/");
        let (start, offered) = grammar
            .complete(&line, line.len(), &context)
            .expect("completing never fails");
        assert_eq!(&line[start..], format!("{here}/"));
        let mut names = inserted(&offered);
        names.sort_unstable();
        assert_eq!(
            names,
            [format!("{here}/drafts/"), format!("{here}/flow.json")]
        );

        // Mid-word, narrowed by the listing rather than by the editor. A
        // directory keeps its separator, so the next Tab carries on inside it.
        let line = format!("read {here}/dra");
        let (_, offered) = grammar
            .complete(&line, line.len(), &context)
            .expect("completing never fails");
        assert_eq!(inserted(&offered), [format!("{here}/drafts/")]);

        // The same at the two other positions the grammar gives a path: an
        // option's value and the one `write` takes.
        for line in [
            format!("add agent draft --file {here}/fl"),
            format!("write {here}/fl"),
        ] {
            let (_, offered) = grammar
                .complete(&line, line.len(), &context)
                .expect("completing never fails");
            assert_eq!(inserted(&offered), [format!("{here}/flow.json")], "{line}");
        }
    }
}
