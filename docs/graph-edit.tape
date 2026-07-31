# Salvor graph authoring tape (vhs). Records docs/graph-edit.gif: build the
# payroll pay-run desk from an empty document at the `salvor graph edit` prompt.
# Two `tool` nodes pull and screen a roster, a `route` branch splits on the
# flagged count, a `review_exceptions` gate parks the flagged arm for a human,
# a `pay_each` map fans out over the roster with `pay_employee` as its body, and
# a `notify_summary` agent writes the closing notice. The agent's `--file`
# resolves to the `sha256:` content hash the document carries, the validator
# names one typoed edge target precisely and passes once it is fixed, and the
# document is written out. The written document is the SAME graph as
# examples/payroll/payroll-run.json: equal canonical hash (proven by posting both
# to /v1/graphs/validate and getting one hash back).
#
# Record with:   vhs docs/graph-edit.tape   (from the repository root)
#
# Privacy: everything runs under a scratch HOME (/tmp/salvor-graph-demo) with a
# generic prompt, so no username, hostname, or real home path is ever on screen.
#
# Hermetic: the session needs no network and no API key, because the editor
# drives no model. The one line that reaches outside the editor at all is the
# agent node's `--file`, and resolving that path to a `sha256:` hash builds the
# definition locally, speaking only to the in-repo payroll-desk MCP server the
# agent definition names (`python3 examples/payroll/server.py`, standard library
# only). Nothing is called; only its tool contract is read.

Output docs/graph-edit.gif

Set Shell "bash"
Set FontSize 16
Set Width 1440
Set Height 1080
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 18ms

# --- hidden setup: build, sanitized HOME, clean prompt ------------------------
# Runs from the repository root (where vhs is invoked), so the copies below read
# the built binary and the demo assets straight out of the repo before switching
# to the scratch HOME. The build is scoped to the one crate the editor needs;
# building the whole workspace here is what broke an earlier recording.
Hide
Type `cargo build --quiet -p salvor-cli` Enter
Type `export HOME=/tmp/salvor-graph-demo` Enter
Type `export PS1='\[\e[38;5;39m\]salvor\[\e[0m\] \[\e[38;5;245m\]>\[\e[0m\] '` Enter
# A real key on the recording machine would defeat the hermetic point; scrub it.
Type `unset ANTHROPIC_API_KEY` Enter
# Fresh scratch tree: a bin dir on PATH, and a work dir holding the agent
# definition plus the target-relative MCP server path that definition names.
Type `rm -rf "$HOME"; mkdir -p "$HOME/bin" "$HOME/work/examples/payroll"` Enter
Type `cp target/debug/salvor "$HOME/bin/"` Enter
# The agent definition declares `python3 examples/payroll/server.py`, resolved
# from the directory salvor runs in, so stage the server at that exact relative
# path under the scratch work dir. It is standard-library Python and is only
# asked for its tool contract while a hash is resolved; nothing is called.
Type `cp examples/payroll/server.py "$HOME/work/examples/payroll/"` Enter
Type `cp examples/payroll/agents/notify-summary.toml "$HOME/work/"` Enter
Type `export PATH="$HOME/bin:$PATH"` Enter
# The MCP handshake that hash resolution performs logs at INFO, which would bury
# the one line worth reading; keep the screen to the editor's own output.
Type `export RUST_LOG=warn` Enter
# `clear` last, so nothing typed above is in the buffer when recording starts.
Type `cd "$HOME/work" && rm -f payroll-run.json && clear` Enter
Show

# --- 1. an empty document, and where it says to start ------------------------
Type "salvor graph edit" Enter
Sleep 1500ms

# --- 2. the two reads that pull and screen the roster ------------------------
# A `tool` node names its id and the MCP tool it calls. `pull_roster` fetches the
# pay period's roster; `flag_exceptions` scans it for amounts far off the median.
Type "add tool pull_roster pull_roster" Enter
Sleep 900ms
Type "add tool flag_exceptions flag_exceptions" Enter
Sleep 900ms

# --- 3. the branch, and its two cases ----------------------------------------
# The branch splits on the flagged count. The paths carry the `structuredContent.`
# prefix because the routed value is a TOOL result, and Salvor records the whole
# tool result as the node's output; the typed fields a branch reads live under
# `structuredContent`. Each case is its own `case` line, added in author order.
Type "add branch route" Enter
Sleep 900ms
Type `case route pay_all --when "structuredContent.flagged_count == 0"` Enter
Sleep 1200ms
Type `case route review --when "structuredContent.flagged_count > 0"` Enter
Sleep 1200ms
# Read the branch back: two cases, each with the expression that selects it.
Type "show route" Enter
Sleep 2800ms

# --- 4. the gate, with its real approval shape -------------------------------
# The approval schema is a JSON argument, and a JSON argument needs no quoting:
# the tokenizer reads from the opening brace until the braces balance, so the
# whole object is one token. Single quotes wrap the WHOLE line here, because the
# prompt uses backticks and the schema uses double quotes; single-quoting leaves
# both untouched. The gate's answer is the map's input, so this shape is the pay
# instruction's shape: a `structuredContent.roster` of amended rows, plus an
# approver.
Type 'add gate review_exceptions --approval-schema {"type":"object","properties":{"structuredContent":{"type":"object","properties":{"roster":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"name":{"type":"string"},"amount_cents":{"type":"integer"},"pay_period":{"type":"string"}},"required":["id","amount_cents"]}}},"required":["roster"]},"approver":{"type":"string"},"note":{"type":"string"}},"required":["structuredContent","approver"]} --prompt "This roster has rows flagged as far off the median (see the flagged list on the routed value). Answer with the amended roster to pay: an object `structuredContent` holding a `roster` array of every employee to pay, each with id, name, and the corrected amount_cents and pay_period, plus an `approver`. The amounts you send are the amounts that get paid. The run stays parked until you answer."' Enter
Sleep 2800ms

# --- 5. the fan-out, and its body --------------------------------------------
# A `map` iterates over a list and runs each element through a body node. `--over`
# is the path to the list (again into a tool result), `--concurrency 1` runs the
# iterations in order, and `--body pay_employee` names the node each iteration
# runs. The body is referenced, not edged: it has no edges of its own, so validate
# will list it as both entry AND terminal, which is correct for a map body.
Type "add map pay_each --over structuredContent.roster --concurrency 1 --body pay_employee" Enter
Sleep 1500ms
Type "add tool pay_employee pay_employee" Enter
Sleep 900ms

# --- 6. one path becomes a content hash --------------------------------------
# A graph names an agent by hash and never by path, because a run's log records
# only the hash. `--file` is the author's convenience: the host builds the
# definition, connecting to the MCP server it declares to read that server's tool
# contract, and hands back the `sha256:` hash the document will carry.
Type "add agent notify_summary --file notify-summary.toml" Enter
Sleep 4s

# --- 7. the edges that make this a shape -------------------------------------
# Added in the reference document's exact order, because an edge array is
# load-bearing for the canonical hash. The two edges out of the branch carry a
# label naming the case each realizes.
Type "edge pull_roster flag_exceptions" Enter
Sleep 800ms
Type "edge flag_exceptions route" Enter
Sleep 800ms
Type "edge route pay_each --label pay_all" Enter
Sleep 900ms
Type "edge route review_exceptions --label review" Enter
Sleep 900ms
Type "edge review_exceptions pay_each" Enter
Sleep 800ms
# A deliberate typo in the last edge's target. An edge to a node that does not
# exist is allowed while a document is built one line at a time, so the mistake
# lands quietly and it is `validate` that names it: the missing id, and the near
# id it is one letter from.
Type "edge pay_each notify_summry" Enter
Sleep 1500ms

# --- 8. the validator names the problem, then passes once it is fixed --------
Type "validate" Enter
Sleep 3500ms
Type "rm edge pay_each notify_summry" Enter
Sleep 1200ms
# Re-adding appends to the end, which is exactly where this edge belongs, so the
# fixed array is back in reference order.
Type "edge pay_each notify_summary" Enter
Sleep 1200ms
Type "validate" Enter
Sleep 3s

# --- 9. the document read back as an outline ---------------------------------
Type "show" Enter
Sleep 5s

# --- 10. save, then leave: there is no `quit`, end of input is how it ends -----
Type "write payroll-run.json" Enter
Sleep 2500ms
Ctrl+D
Sleep 1500ms

# --- 11. the written file stands on its own ----------------------------------
Type "salvor graph validate payroll-run.json" Enter
Sleep 4s
