# Salvor graph authoring tape (vhs). Records docs/graph-edit.gif: build a graph
# document from an empty one at the `salvor graph edit` prompt, resolve an agent
# definition path to a content hash, complete a node id at a Tab from the
# document being built, let the validator name a real problem and then pass once
# it is fixed, write the document out, and validate the written file on its own.
#
# Record with:   vhs docs/graph-edit.tape   (from the repository root)
#
# Privacy: everything runs under a scratch HOME (/tmp/salvor-graph-demo) with a
# generic prompt, so no username, hostname, or real home path is ever on screen.
#
# Hermetic: the session needs no network and no API key, because the editor
# drives no model. The one line that reaches outside the editor at all is the
# agent node's `--file`, and resolving that path to a `sha256:` hash builds the
# definition locally, speaking only to the in-repo salvor-demo-research MCP
# server the file names.

Output docs/graph-edit.gif

Set Shell "bash"
Set FontSize 16
Set Width 1440
Set Height 1080
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 30ms

# --- hidden setup: build, sanitized HOME, clean prompt ------------------------
# Runs from the repository root (where vhs is invoked), so the copies below read
# the built binaries and the demo assets straight out of the repo before
# switching to the scratch HOME.
Hide
Type `cargo build --quiet` Enter
Type `export HOME=/tmp/salvor-graph-demo` Enter
Type `export PS1='\[\e[38;5;39m\]salvor\[\e[0m\] \[\e[38;5;245m\]>\[\e[0m\] '` Enter
# A real key on the recording machine would defeat the hermetic point; scrub it.
Type `unset ANTHROPIC_API_KEY` Enter
# Fresh scratch tree: a bin dir on PATH, and a work dir holding the agent
# definition plus the target-relative MCP server path that definition names.
Type `rm -rf "$HOME"; mkdir -p "$HOME/bin" "$HOME/work/target/debug"` Enter
Type `cp target/debug/salvor "$HOME/bin/"` Enter
Type `cp target/debug/salvor-demo-research "$HOME/work/target/debug/"` Enter
# demo/agent.toml under the name the editor's own documented example uses.
Type `cp demo/agent.toml "$HOME/work/research.toml"` Enter
Type `export PATH="$HOME/bin:$PATH"` Enter
# The MCP handshake that hash resolution performs logs at INFO, which would bury
# the two lines worth reading; keep the screen to the editor's own output.
Type `export RUST_LOG=warn` Enter
# `clear` last, so nothing typed above is in the buffer when recording starts.
Type `cd "$HOME/work" && rm -f pipeline.json && clear` Enter
Show

# --- 1. an empty document, and where it says to start ------------------------
Type "salvor graph edit" Enter
Sleep 2s
Type "show" Enter
Sleep 4s

# --- 2. a path becomes a content hash ----------------------------------------
# A graph names an agent by hash and never by path, because a run's log records
# only the hash. `--file` is the author's convenience: the host builds the
# definition and hands back the hash the document will carry.
Type "add agent research --file research.toml" Enter
Sleep 5s

# --- 3. a gate, and the edges that make this a shape -------------------------
Type `add gate approve --approval-schema {"type":"object"}` Enter
Sleep 3s
# Tab is answered from the document as it stands, so the ids it offers are the
# two just added and no fixed list could have known them. Both ends of this edge
# are typed at partially and completed. `research` and `approve` share no prefix,
# so an empty position has nothing to insert on the first Tab and prints the pair
# on the second, while a partial id that matches one of them completes on the
# first: the pauses below are sized for that, not decoration.
Type "edge "
Sleep 800ms
Tab
Sleep 300ms
Tab
Sleep 2s
Type "res"
Sleep 500ms
Tab
Sleep 1500ms
Type " app"
Sleep 500ms
Tab
Sleep 1500ms
Enter
Sleep 2s
# An edge may be drawn to a node that does not exist yet, which is why `publish`
# below is typed in full: Tab has nothing to offer for a node the document does
# not hold. A document built one line at a time is momentarily wrong, so
# `validate` is a question the author asks when ready rather than a gate on
# every edit.
Type "edge approve publish" Enter
Sleep 3500ms

# --- 4. the validator names the problem, then passes once it is fixed --------
Type "validate" Enter
Sleep 5s
Type "add tool publish record_finding --input note=research.output.summary" Enter
Sleep 3s
Type "validate" Enter
Sleep 5s

# --- 5. the document read back as an outline ---------------------------------
Type "show" Enter
Sleep 6s

# --- 6. save, then leave: there is no `quit`, end of input is how it ends -----
Type "write pipeline.json" Enter
Sleep 4s
Ctrl+D
Sleep 2s

# --- 7. the written file stands on its own -----------------------------------
Type "salvor graph validate pipeline.json" Enter
Sleep 5s
