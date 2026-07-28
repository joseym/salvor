# Salvor demo tape (vhs). Records docs/demo.gif: start a durable research run,
# kill -9 it mid-flight, resume it, and prove exactly nine findings were
# written with zero duplicates.
#
# Record with:   vhs docs/demo.tape   (from the repository root)
#
# Privacy: everything runs under a scratch HOME (/tmp/salvor-demo) with a
# generic prompt, so no username, hostname, or real home path is ever on
# screen. The whole run is hermetic: model calls go to salvor-demo-model, a
# scripted local server that serves the same twenty-turn conversation as the
# demo_run integration test, so the recording needs no network and no API
# key. To record against the real Anthropic endpoint instead, unset
# SALVOR_DEMO_BASE_URL in the hidden setup and export ANTHROPIC_API_KEY; the
# same demo/agent.toml drives both, and its base_url_env hook does the switch.

Output docs/demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 1440
Set Height 800
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 30ms

# --- hidden setup: build, sanitized HOME, scripted model, clean prompt -------
# Runs from the repository root (where vhs is invoked), so the copies below
# read the built binaries and the demo assets straight out of the repo before
# switching to the scratch HOME.
Hide
Type `cargo build --quiet` Enter
Type `export HOME=/tmp/salvor-demo` Enter
Type `export PS1='\[\e[38;5;39m\]salvor\[\e[0m\] \[\e[38;5;245m\]>\[\e[0m\] '` Enter
# A real key on the recording machine would defeat the hermetic point; scrub it.
Type `unset ANTHROPIC_API_KEY` Enter
# Fresh scratch tree: a bin dir on PATH, and a work dir holding demo/ plus the
# target-relative MCP server path demo/agent.toml names.
Type `rm -rf "$HOME"; mkdir -p "$HOME/bin" "$HOME/work/target/debug" "$HOME/work/demo"` Enter
Type `cp target/debug/salvor target/debug/salvor-demo-model "$HOME/bin/"` Enter
Type `cp target/debug/salvor-demo-research "$HOME/work/target/debug/"` Enter
Type `cp demo/agent.toml demo/input.json "$HOME/work/demo/"` Enter
Type `export PATH="$HOME/bin:$PATH"` Enter
# The scripted model: 700ms per turn so the run is watchable and killable, and
# because that delay sits on the model call (tool calls are instant local file
# writes) a kill almost always lands in the recoverable awaiting-model state.
Type `salvor-demo-model --port 8899 --delay-ms 700 >/tmp/salvor-demo-model.log 2>&1 &` Enter
# The agent file's base_url_env hook reroutes every model call here; no key.
Type `export SALVOR_DEMO_BASE_URL=http://127.0.0.1:8899` Enter
Type `export RUST_LOG=warn` Enter
# Wait for the server's listening socket before recording anything.
Type `until (exec 3<>/dev/tcp/127.0.0.1/8899) 2>/dev/null; do sleep 0.1; done` Enter
Type `cd "$HOME/work" && rm -f findings.txt salvor.db && clear` Enter
Show

# --- 1. the agent: model, prompt, and the real/mock switch -------------------
Type "head -16 demo/agent.toml" Enter
Sleep 3s

# --- 2. start a durable run in the background and grab its pid ----------------
# salvor prints the run id first, before any step executes, so it survives a
# kill. The run then drives itself, appending one line per finding.
Type "salvor run --agent demo/agent.toml --input @demo/input.json &" Enter
Sleep 1s
Type "SALVOR_PID=$!" Enter
Sleep 3s

# --- 3. findings are landing: the run is really executing writes -------------
Type "cat findings.txt" Enter
Sleep 2s

# --- 4. kill -9 the run, mid-flight ------------------------------------------
Type "kill -9 $SALVOR_PID" Enter
Sleep 2s

# --- 5. the crashed run is still in the store, awaiting a model call ----------
# `--group progress` asks the question an operator actually has after a crash:
# which runs are unfinished? A killed run folds to awaiting-model, so it is the
# one this returns. The status column is coloured by the same grouping.
Type "salvor list --group progress" Enter
Sleep 4s

# Capture the run id mechanically for the resume command (it is a fresh UUID
# each recording; the filtered list above shows that same id on screen).
#
# Hide only stops vhs recording frames, so the shell's echo of this line would
# still be sitting in the buffer when Show resumes. The trailing printf walks
# the cursor back over the echoed line and clears from there down, and the next
# prompt draws in its place.
Hide
Type `RUN_ID=$(salvor list | awk 'NR==2 {print $1}'); printf '\033[1A\033[J'` Enter
Show

# --- 6. resume: replay the log, run only the unrecorded steps ----------------
Type 'salvor resume "$RUN_ID" --agent demo/agent.toml' Enter
Sleep 7s

# --- 7. one continuous event log, ending in a clean completion ---------------
Type 'salvor history "$RUN_ID" | tail' Enter
Sleep 4s

# --- 8. exactly nine findings: nothing was written twice ---------------------
Type "wc -l findings.txt" Enter
Sleep 4s

# --- 9. and the run now reads as finished -------------------------------------
Type "salvor list --status completed" Enter
Sleep 4s
