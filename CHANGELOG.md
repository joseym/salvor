# Changelog
All notable changes to this project will be documented in this file. See [conventional commits](https://www.conventionalcommits.org/) for commit guidelines.

- - -
## v0.10.0 - 2026-08-30
#### Features
- (**bridge**) one-click label for a branch edge with no label - (60a90de) - Josey Morton
- (**bridge**) an overdue sleeping run stands out in the runs list - (2c94b65) - Josey Morton
- (**bridge**) one-click fix relabels a branch edge to its missing case - (56eb0b6) - Josey Morton
- (**bridge**) an overdue sleeping run says so - (6954782) - Josey Morton
- (**bridge**) one-click fix routes an unrouted branch case to a terminal node - (ad4c285) - Josey Morton
- (**bridge**) a signal wait is progress, never an inbox card - (efa0645) - Josey Morton
- (**bridge**) the workflows canvas knows the delay node - (ddcafe9) - Josey Morton
- (**bridge**) a sleeping run reads as progress, never as attention - (2e621b3) - Josey Morton
- (**cli**) salvor anchor and salvor verify --against - (9c96edc) - Josey Morton
- (**cli**) a sleeping run wakes itself, by cron or by the server - (139e52e) - Josey Morton
- (**config**) an agent file declares the shape its answers take - (e46c1be) - Josey Morton
- (**engine**) a tool and a graph node are the two ways to start a timer - (fc5fe69) - Josey Morton
- (**engine**) a fold's bound can be a failure, its value is the payload, and a dead run says so - (70ee511) - Josey Morton
- (**engine**) drive the fold loop the format has promised all along - (a4a42e7) - Josey Morton
- (**examples**) langchain desk proves declared keys and a refused resolve - (f6badce) - Josey Morton
- (**examples**) a LangChain support desk that survives a crash - (1559be6) - Josey Morton
- (**examples**) the accounts desk can wait on the payment webhook - (44f8e73) - Josey Morton
- (**examples**) a run that sleeps for real, and salvor wake finishes it - (d29302c) - Josey Morton
- (**examples**) a refine loop that converges, survives a kill, and refuses to settle - (2e46e53) - Josey Morton
- (**graph**) a branch edge must have a label - (b9d1baa) - Josey Morton
- (**graph**) a branch edge must name a declared case - (33b77a2) - Josey Morton
- (**graph**) a branch case must name an outbound edge - (c5d4b30) - Josey Morton
- (**graph**) a fold says what a reached bound means, and a dead reference fails at submit - (1c9787a) - Josey Morton
- (**llm**) let a request constrain which tool the model calls - (e4e1dfb) - Josey Morton
- (**replay**) a run can record that it is sleeping until an instant - (e701e6b) - Josey Morton
- (**replay**) record and replay the fold markers through the cursor - (07a4e75) - Josey Morton
- (**runtime**) the schema subset honours numeric, length and item bounds - (6e098e4) - Josey Morton
- (**runtime**) a suspension says whether a signal or a person answers it - (6a2603c) - Josey Morton
- (**runtime**) a run can sleep until an instant, and a wait can name what it waits on - (0c3f1a0) - Josey Morton
- (**runtime**) an agent with a declared shape answers through a forced tool call - (3f5ad5c) - Josey Morton
- (**sdk-py**) abandon a run from the client, as TypeScript can - (0c39665) - Josey Morton
- (**sdk-py**) record a tool body that raises as the call's failure - (1b6c7d1) - Josey Morton
- (**sdk-python**) the middleware works under invoke and ainvoke - (e6e6c07) - Josey Morton
- (**sdk-python**) a LangChain middleware that makes an agent durable - (9f3f102) - Josey Morton
- (**sdk-python**) client_model_intent and client_model_completion - (82303c1) - Josey Morton
- (**sdk-python**) an async client and driver over one shared core - (45f7449) - Josey Morton
- (**sdk-ts**) record a tool body that throws as the call's failure - (920d54a) - Josey Morton
- (**sdk-typescript**) an unconfirmed tool stops the agent for a person - (c9c7f02) - Josey Morton
- (**sdk-typescript**) finishThread and currentToolCall - (ebea552) - Josey Morton
- (**sdk-typescript**) a LangChain middleware that makes an agent durable - (901cbe9) - Josey Morton
- (**sdk-typescript**) clientModelIntent and clientModelCompletion - (64a7f6e) - Josey Morton
- (**sdks**) RunStatus carries overdue and overdue_seconds - (1ccfd2b) - Josey Morton
- (**sdks**) client-driven drivers can sleep and wake themselves - (2fe8199) - Josey Morton
- (**server**) resolve checks the declaration, a client tool can fail - (6d2208e) - Josey Morton
- (**server**) a client-run lease can be released and kept alive - (b27c9de) - Josey Morton
- (**server**) a client-run lease is held until it lapses - (7605644) - Josey Morton
- (**server**) a client-driven run records a model call the client made - (5476579) - Josey Morton
- (**server**) a client-driven run may sleep, and its client wakes it - (4a64d45) - Josey Morton
- (**server**) overdue sleepers say so, and the sweeper warns once - (ec1d2dd) - Josey Morton
- (**store**) read every run's chain head and the hash at a position - (ce9ff2f) - Josey Morton
- (**tools**) an MCP tool parks its run through _meta - (27915e9) - Josey Morton
#### Bug Fixes
- (**bridge**) start refusal names the code itself - (eeaff2a) - Josey Morton
- (**cli**) anchor reads every log before writing - (04c2352) - Josey Morton
- (**cli**) read-only verbs never create a store; list sees deleted rows - (c115464) - Josey Morton
- (**cli**) anchor and verify refuse what they cannot check - (0134d94) - Josey Morton
- (**cli**) resolve tells a client-driven run's owner what happens next - (c0bd53c) - Josey Morton
- (**cli**) salvor wake leaves client-driven runs alone - (116fe9c) - Josey Morton
- (**cli**) the unmet-tool refusal says the whole document is checked - (f96f082) - Josey Morton
- (**cli**) a real wake names the remedy its dry run already did - (005feba) - Josey Morton
- (**cli**) wake names its sweep instant and the next due run - (6cc8b3f) - Josey Morton
- (**cli**) wake checks every file given and survives an unreadable log - (a48ade9) - Josey Morton
- (**cli**) hints carry --store, list shows wake time, two-unit durations - (115f5ea) - Josey Morton
- (**cli**) a run taken by another driver is named by its settled state only - (794c9eb) - Josey Morton
- (**cli**) salvor wake reports what actually happened - (f2ce50a) - Josey Morton
- (**cli**) a named but empty auth variable refuses to serve - (e5c98c6) - Josey Morton
- (**cli**) the resolve report names the whole resume command a graph run needs - (a6de1b5) - Josey Morton
- (**engine**) a fold folds bare payloads entering or produced, and every failure names itself - (f23eaf1) - Josey Morton
- (**errors**) print run ids and sequence numbers plainly - (54c006b) - Josey Morton
- (**examples**) follow-up expects the hint the CLI now prints - (7bb4245) - Josey Morton
- (**examples**) follow-up passes the invoice id and nothing else - (72d63a8) - Josey Morton
- (**examples**) follow-up says where its ledgers land and numbers by seq - (fcaa7ec) - Josey Morton
- (**sdk**) a recorded failure survives a fork, so the refusal says so - (2719360) - Josey Morton
- (**sdk**) only a write that throws is recorded as the call's failure - (977320c) - Josey Morton
- (**sdk**) refusals quote the tool's error and stop repeating the code - (b5cca20) - Josey Morton
- (**sdk**) an abandoned thread is refused by name in both middlewares - (3aa856a) - Josey Morton
- (**sdk-python**) server refusals reach salvor_error with their code - (e9508f7) - Josey Morton
- (**sdk-python**) lease release, heartbeat and error codes - (4537814) - Josey Morton
- (**sdk-python**) one driver per thread under the held lease - (f0102cb) - Josey Morton
- (**sdk-python**) tool results replay byte for byte, forks are marked - (a572a8f) - Josey Morton
- (**sdk-typescript**) server refusals reach salvorError with their code - (c32c9ee) - Josey Morton
- (**sdk-typescript**) lease release, heartbeat and error codes - (be1bb17) - Josey Morton
- (**sdk-typescript**) one driver per thread under the held lease - (6533b28) - Josey Morton
- (**sdk-typescript**) a refused re-open no longer blames a restart - (b11dee6) - Josey Morton
- (**sdk-typescript**) tool results replay byte for byte, forks are marked - (e970186) - Josey Morton
- (**sdk-typescript**) parallel tool calls record in the model's order - (2916f70) - Josey Morton
- (**server**) a client-driven run's log stays readable without a lease - (22b9d77) - Josey Morton
- (**server**) a restart no longer strands a client-driven run - (a6f29d8) - Josey Morton
- (**server**) resume refuses a client-driven run - (d65c9f8) - Josey Morton
- (**server**) a lost race is not an error, and the sweeper says what to do - (befd237) - Josey Morton
- (**server**) a wire timestamp is never an empty string - (0af2323) - Josey Morton
- (**server**) the wake sweeper leaves client-driven runs alone - (3c4309c) - Josey Morton
- (**store**) two chain errors for a head-length mismatch and deleted rows - (56e2894) - Josey Morton
- (**store**) read a log's rows and its chain head from one snapshot - (e45b639) - Josey Morton
- (**store**) a position conflict names the run and seq plainly - (a261eef) - Josey Morton
- (**store**) a writer waits for another writer instead of failing at once - (8307e39) - Josey Morton
- (**tools**) a malformed park request fails the call once, on any effect - (7b8cf76) - Josey Morton
#### Documentation
- (**sdk**) fork notice fields, an error table for TypeScript, schema fix forks - (df44de7) - Josey Morton
- (**sdk**) abandon sits beside resolve in both quick references - (9c8dab2) - Josey Morton
- (**server**) the API page names the SDKs without a version - (78a6954) - Josey Morton
- a nightly script that takes its first anchor, never overwrites one - (d865b97) - Josey Morton
- nightly order, read-only verbs, and what an anchor cannot mark - (306b0ce) - Josey Morton
- where to keep the anchor, how often to take one, and exit codes - (89d0e52) - Josey Morton
- the anchor file is unsigned - (f0b4eb3) - Josey Morton
- say which schema keywords count where a declaration is written - (323bef1) - Josey Morton
- a fork re-runs the writes, and only a failed write is recorded - (9b955e4) - Josey Morton
- a run waiting on a person waits until someone acts - (900746e) - Josey Morton
- key fields name what makes two calls the same, one serve per store - (eada1c2) - Josey Morton
- the client-tool declaration's idempotency_key field in the README - (d8f9e91) - Josey Morton
- the fork seq is the first position that differed - (5f9dc18) - Josey Morton
- a malformed park request is never retried - (dcbd890) - Josey Morton
- parking from MCP, client-driven sleep, overdue runs, branch edges - (138ac40) - Josey Morton
- show the Suspended event payload an SSE consumer receives - (3ff0055) - Josey Morton
- point the MCP reassurance at the limit that follows it - (56fa2e3) - Josey Morton
- how a signal wait is resumed, and who can park a run at all - (d6481a2) - Josey Morton
- what the wake sweeper can and cannot wake - (4d65fde) - Josey Morton
- how a run sleeps and what wakes it - (ae287de) - Josey Morton
- the operations page describes an auth flag that now refuses to start - (b1f802c) - Josey Morton
- close the gaps the operator pages left open - (f6649ef) - Josey Morton
- count fold among the node kinds a graph runs - (2e8b110) - Josey Morton
- say what the log keeps and why erasure cannot be granular - (f34d0c7) - Josey Morton
- state the TLS, backup, and retention story an operator needs - (0b4e2ad) - Josey Morton
#### Tests
- (**replay**) a gate wait counts and a sleep does not, in one log - (2857d3f) - Josey Morton
- (**sdk-python**) a salvor restart mid-invoke is survived - (ff8ef55) - Josey Morton
- (**sdk-typescript**) a salvor restart mid-invoke is survived - (ad3b52d) - Josey Morton
#### Build system
- (**deps**) bump @hono/node-server in /examples/typescript-tools - (c242529) - dependabot[bot]
- (**deps**) bump fast-uri in /examples/typescript-tools - (c9e3337) - dependabot[bot]
- (**deps**) bump hono in /examples/typescript-tools - (558c277) - dependabot[bot]
- (**deps**) bump ip-address in /examples/typescript-tools - (637fb16) - dependabot[bot]
- (**deps**) bump @hono/node-server and @modelcontextprotocol/sdk - (ee521e2) - dependabot[bot]

- - -

## v0.9.2 - 2026-08-03
#### Features
- (**wasm**) export the real history renderer, agent parse, and budget check - (8f47306) - Josey Morton
#### Bug Fixes
- (**tools**) an MCP server dies with the connection that spawned it - (49f3827) - Josey Morton
#### Documentation
- count v0.9.1 among the shipped releases - (0aaa2c9) - Josey Morton
#### Tests
- commit the budget fold fixtures the suite reads - (3332eee) - Josey Morton
#### Build system
- stamp every internal version requirement the bump was missing - (3bc3741) - Josey Morton

- - -

## v0.9.1 - 2026-08-02
#### Features
- (**salvor**) the umbrella crate compiles the front-page sample - (4397ece) - Josey Morton
#### Bug Fixes
- (**server**) a client-run 401 names the server-side key variable - (94c8173) - Josey Morton
#### Documentation
- count v0.9.0 among the shipped releases - (6a70725) - Josey Morton
#### Continuous Integration
- wait as long for the release as the release actually takes - (4746f31) - Josey Morton

- - -

## v0.9.0 - 2026-08-02
#### Features
- (**engine**) refuse a gate approval its schema rejects - (fa53cfe) - Josey Morton
- (**runtime**) let a declared idempotency key hold across runs - (dc396ac) - Josey Morton
- (**store**) make the event log tamper-evident, for real - (745b3a3) - Josey Morton
- (**tools**) declare an idempotency key in the agent file - (e006c12) - Josey Morton
#### Bug Fixes
- (**cli**) the seven paper cuts a field-test round surfaced - (16f6c59) - Josey Morton
- (**npm**) install a killable binary, not a Node wrapper - (b4ec79d) - Josey Morton
#### Documentation
- make the idempotency key findable from where it is promised - (e9ba9a7) - Josey Morton
- say how the key, the npm binary, and the cross-run guarantee work - (509bbcd) - Josey Morton
- point the readme at salvor.run - (d2bdd9f) - Josey Morton
#### Build system
- regenerate release facts at bump time, accept the one-ahead state - (0d68cd0) - Josey Morton

- - -

## v0.8.0 - 2026-08-01
#### Features
- (**bridge**) draw the body reference a map only implies - (df30546) - Josey Morton
- (**bridge**) draw and edit graphs on the canvas - (d4bb2fb) - Josey Morton
- (**bridge**) open a graph document on the canvas - (1157079) - Josey Morton
- (**cli**) serve a demo model script from a file - (6adc7ea) - Josey Morton
- (**cli**) name the agent file check `agent validate` - (e0c8a3c) - Josey Morton
- (**cli**) complete file paths in the graph editor - (dc66029) - Josey Morton
- (**cli**) complete graph edit lines from the document being built - (db6a840) - Josey Morton
- (**cli**) put the graph editor behind a prompt - (5f06355) - Josey Morton
- (**cli**) a graph editor that folds commands into a document - (8eb45d1) - Josey Morton
- (**cli**) print an agent definition's hash - (6c09274) - Josey Morton
- (**core**) record who performed a tool call - (5a450b4) - Josey Morton
- (**examples**) a payroll run that cannot pay anyone twice - (1b24b77) - Josey Morton
- (**examples**) drive a graph from three client applications - (f2ec337) - Josey Morton
- (**examples**) drive a refund dispute through a graph - (bdfb0e7) - Josey Morton
- (**graph**) check the document schema in and gate it - (a05ee71) - Josey Morton
- (**replay**) mark a client-performed call where a person reads it - (c6768ef) - Josey Morton
- (**sdk**) read the pinned fields, assume the safe trust default - (1f703f8) - Josey Morton
- (**sdk**) refuse a bad gate schema at build time, say when an intent is settled - (a4643e1) - Josey Morton
- (**sdk**) reach client-performed tools from Python and TypeScript - (7bbc100) - Josey Morton
- (**server**) pin declared fields on a client completion, distrust silence - (1524ed8) - Josey Morton
- (**server**) let a client run a tool and record it - (cdd5a7f) - Josey Morton
- (**tools**) let a tool declare the shape of its completion - (b2aa6de) - Josey Morton
- start a graph run from the SDKs and the canvas - (7048fcb) - Josey Morton
#### Bug Fixes
- (**cli**) hold the MCP handshake to warn for every command - (9dd6239) - Josey Morton
- (**cli**) demo-model help text, quiet hash output, deduplicated errors - (ec5ba0f) - Josey Morton
- (**examples**) keep send_notice working on an empty notices file - (ff7a599) - Josey Morton
- (**graph**) explain a malformed branch condition in the document's own terms - (e8cd485) - Josey Morton
- (**sdk**) let a branch name the agent that decides a case - (72e2ecf) - Josey Morton
#### Documentation
- (**examples**) a refund desk where the money moves in the client - (6fe95d9) - Josey Morton
- (**examples**) tighten the graph-service readme - (00d95cb) - Josey Morton
- (**sdks**) document the graph flow in both SDK READMEs - (44b9bc3) - Josey Morton
- put graphs and client tools on the front page, prove every command - (3c07b59) - Josey Morton
- record the payroll desk twice, prove it is one document - (f84a6ef) - Josey Morton
- say how to back up the store, warn about the tape before the walkthrough - (40a51ea) - Josey Morton
- answer what five testers could not - (574a97d) - Josey Morton
- fix the dash punctuation in the bridge - (eb04cee) - Josey Morton
- fix the dash punctuation in the Rust crates and the SDKs - (aca930b) - Josey Morton
- point editors at the graph schema by file pattern - (bb19ba7) - Josey Morton
- stop the graph docs saying nothing runs a graph - (551fab1) - Josey Morton
- record the graph editor building a document from nothing - (b955bde) - Josey Morton
- correct the release doc and drop the last em-dashes - (dd72099) - Josey Morton
- drop the tells from the readme opening - (54047d3) - Josey Morton
#### Tests
- (**engine**) kill a graph run at every boundary and resume it - (d2b2c11) - Josey Morton
#### Build system
- track the editor schema mapping for graph documents - (8e861ae) - Josey Morton
#### Continuous Integration
- pin wasm-pack instead of taking whatever latest resolves to - (2ba60f2) - Josey Morton
- prove the CLI's pure half still builds for wasm32 - (6a2970d) - Josey Morton
- generate the release facts and fail when they drift - (79aafb5) - Josey Morton
#### Miscellaneous Chores
- ignore compiled python bytecode - (03f6cbf) - Josey Morton

- - -

## v0.7.0 - 2026-07-29
#### Features
- (**cli**) wrap the reports to a width the caller chooses - (ecd96ad) - Josey Morton
- (**cli**) expose the parser and renderer to the browser - (767fcc8) - Josey Morton
#### Refactoring
- (**cli**) split the parse tree and renderer into salvor-cli-core - (1c18362) - Josey Morton
- move the pure view types down into the replay leaf - (a3d3eca) - Josey Morton

- - -

## v0.6.0 - 2026-07-28
#### Features
- (**cli**) complete real run ids and agents from the store - (ade1e23) - Josey Morton
- (**cli**) describe the CLI surface in a checked-in manifest, gated against drift - (09c5d0a) - Josey Morton
- (**container**) publish an API-only image with a durable store volume - (52eb350) - Josey Morton
#### Bug Fixes
- (**release**) stamp the version everywhere a bump has to reach - (4bc9b63) - Josey Morton
#### Documentation
- (**examples**) point the hero and index docs at the published packages - (bb32155) - Josey Morton
- describe the release pipeline as one that has shipped - (93a740d) - Josey Morton
- re-record the demo with the list filters - (9f4a7a8) - Josey Morton
- stop pinning a version in the README and name the new install routes - (44852d7) - Josey Morton
#### Build system
- ship statically linked musl binaries and gate them in CI - (b661081) - Josey Morton
#### Style
- satisfy rustfmt and the nested-format lint in the CLI - (928de03) - Josey Morton

- - -

## v0.5.3 - 2026-07-28
#### Features
- (**cli**) complete and validate the list filters, and add shell completions - (a0a2cbd) - Josey Morton
- (**cli**) colour the list table by what each status asks of the reader - (6d076fe) - Josey Morton
- (**cli**) run a self-contained fixture directory offline - (586f1da) - Josey Morton
- (**cli**) add the salvage-claim MCP server for the hero fixture - (af6906a) - Josey Morton
- (**examples**) add the hero fixture behind salvor run --fixture - (412d4b8) - Josey Morton
#### Bug Fixes
- (**examples**) run the polyglot Python app against the published package - (169b1fd) - Josey Morton
- (**examples**) install the published client and stop assuming one node path - (b02b826) - Josey Morton
- (**examples**) let the polyglot example run on ports a real server is not using - (8a4a45e) - Josey Morton
- (**sdks**) correct the TypeScript example against the real client API - (d1c5e52) - Josey Morton
- (**server**) make the embedded dashboard opt-in so the crate builds from the registry - (9eee8f3) - Josey Morton
#### Documentation
- (**examples**) install the published packages instead of building from source - (14cc849) - Josey Morton
#### Tests
- (**cli**) prove the hero fixture runs offline and writes once - (ee02257) - Josey Morton
#### Miscellaneous Chores
- (**examples**) track the polyglot TypeScript lockfile - (24813da) - Josey Morton

- - -

## v0.5.2 - 2026-07-27
#### Features
- (**salvor**) make the umbrella crate a real facade over the family - (9659b59) - Josey Morton
- (**sdks**) move the TypeScript client to the salvor-run scope and align SDK versions - (1ded75d) - Josey Morton
#### Bug Fixes
- (**sdks**) correct the Python example and stop exporting an implementation import - (c08c3a0) - Josey Morton
- (**sdks**) keep personal notes out of published packages and read the version from metadata - (5784797) - Josey Morton
#### Documentation
- put the published packages ahead of the checkout in every install path - (a725dd9) - Josey Morton
#### Continuous Integration
- publish the crate family on a version tag with trusted publishing - (977ca3f) - Josey Morton
- publish the TypeScript client to npm on a version tag - (712c813) - Josey Morton
- publish the Python SDK to PyPI on a version tag - (7758102) - Josey Morton
- publish the npm installer package on release - (cc2f0db) - Josey Morton
- keep the injected build steps out of the workflows directory - (f81f204) - Josey Morton
- stop attempting publishes that have no credentials - (e5bbe1e) - Josey Morton
#### Miscellaneous Chores
- remove the superseded Leptos dashboard - (5d86b7d) - Josey Morton

- - -

## v0.5.1 - 2026-07-25
#### Bug Fixes
- (**bridge**) keep the canvas toolbar clear of the minimap - (0143b0d) - Josey Morton
- (**bridge**) stop stating the same fact twice and make the pickers show what is selected - (1703996) - Josey Morton
- (**cli**) treat an unreaped exited server as stopped, not still running - (1d0bc39) - Josey Morton
- (**cli**) give a SIGTERM'd server room to shut down before calling it stuck - (1513911) - Josey Morton
- (**release**) ship the dashboard in released binaries and say so when it is absent - (7cdc02f) - Josey Morton
#### Documentation
- point the quickstart at the published crates - (14d7302) - Josey Morton
- explain what a salvor is - (471c0be) - Josey Morton
- tighten the README and show the Bridge - (833093a) - Josey Morton
#### Build system
- give the remaining internal dependencies the version crates.io requires - (22c80d1) - Josey Morton
- keep the two internal dev-dependency cycles out of published manifests - (6042ee7) - Josey Morton
#### Continuous Integration
- run the Bridge jobs on a Node the Angular CLI supports - (4367e04) - Josey Morton
#### Miscellaneous Chores
- (**cli**) document the lean build on docs.rs - (2b013a2) - Josey Morton
- add crates.io discovery metadata and use the installed binary name in docs - (4c4db61) - Josey Morton
- declare the supported Rust version and add contributor and security docs - (1227183) - Josey Morton

- - -

## v0.5.0 - 2026-07-24
#### Features
- (**agents**) give an agent an optional display name, outside its identity - (60827a4) - Josey Morton
- (**bridge**) require the server's full 64-hex agent hash and police model-decision cases in wf-validate - (bc31083) - Josey Morton
- (**bridge**) render the fold node and its loop progress on the canvas - (42d2286) - Josey Morton
- (**bridge**) first receipts dock, coach-mark, and reopen paths - (02d96e6) - Josey Morton
- (**bridge**) first receipts step state, predicates, and real-signal wiring - (2e0f747) - Josey Morton
- (**bridge**) stage the abandon confirm's entrance and give its receipt an exit - (af9efcc) - Josey Morton
- (**bridge**) derive and surface a stalled run from liveness evidence - (1d23dc2) - Josey Morton
- (**bridge**) surface fork discoverability and lineage - (7d9d6b3) - Josey Morton
- (**bridge**) sign-post waiting runs to the inbox and fix the empty state copy - (8ee4783) - Josey Morton
- (**bridge**) show the running server's build in the About panel - (34fc589) - Josey Morton
- (**bridge**) render the graph format's optional node name - (addbf3b) - Josey Morton
- (**bridge**) un-hold the Workflows view - (44ecf70) - Josey Morton
- (**bridge**) wire the app-wide fork entry points through one door - (950e375) - Josey Morton
- (**bridge**) complete the canvas's deferred pieces - (f300b12) - Josey Morton
- (**bridge**) port the workflows canvas behind the held flag - (c2abc98) - Josey Morton
- (**bridge**) add POST /v1/graphs submission to the graph catalog API - (5e63e65) - Josey Morton
- (**bridge**) graph API layer and a real capabilities probe - (8cb055e) - Josey Morton
- (**bridge**) mark the scrubber fold boundary and follow it while dragging - (b00e654) - Josey Morton
- (**bridge**) replace Last event column with Agent, add grouped view - (f020471) - Josey Morton
- (**bridge**) add agent identity and grouping to the runs model - (5b716f3) - Josey Morton
- (**bridge**) add agent registry service for name resolution - (b3b8881) - Josey Morton
- (**bridge**) make Cmd-K jump to real runs and views, and mirror the keys - (cfaaa29) - Josey Morton
- (**bridge**) hold Workflows out of the shipped build until the graph engine - (b36155f) - Josey Morton
- (**bridge**) Spend, folded from the real log - (ef83d8c) - Josey Morton
- (**bridge**) let another view apply a filter to Runs - (b02ea0e) - Josey Morton
- (**bridge**) give a lone caption its Note eyebrow - (5960940) - Josey Morton
- (**bridge**) let RunRef show an agent hash or a driver label - (7f92413) - Josey Morton
- (**bridge**) wire the Inspector and Inbox views into the shell - (b0a6585) - Josey Morton
- (**bridge**) app shell, path routing and the Runs ledger - (cdc043e) - Josey Morton
- (**bridge**) API service layer over @salvor/client + connection state machine - (9cb8ad3) - Josey Morton
- (**bridge**) add the token-parity page and a build-time CSS gate - (a649b1c) - Josey Morton
- (**bridge**) port the ledger token system and add the theme toggle - (18c1290) - Josey Morton
- (**bridge**) scaffold the Angular workspace with Tailwind wired in - (96dc4b0) - Josey Morton
- (**cli**) add salvor serve --demo-tools - (70f1699) - Josey Morton
- (**cli**) add salvor fork for local fork-from-node - (8bf81ad) - Josey Morton
- (**cli**) add local graph run and graph-aware resume - (e622747) - Josey Morton
- (**cli**) add salvor serve --dev for hot-reloading UI iteration - (1b342b1) - Josey Morton
- (**cli**) thread run labels through run and resume - (963bf07) - Josey Morton
- (**cli**) add salvor serve --kill to find and stop a running server - (173cc82) - Josey Morton
- (**cli**) add a build verb that produces the whole product - (d668804) - Josey Morton
- (**cli**) honor SALVOR_MODEL_BASE_URL in the client-run model executor - (99732a9) - Josey Morton
- (**cli**) add run, resume, list, history, and replay commands - (89022dc) - Josey Morton
- (**core**) add optional labels to RunStarted - (96dbd48) - Josey Morton
- (**core**) add replay cursor, state derivation, and context events - (195e077) - Josey Morton
- (**core**) add the versioned event model - (d083a94) - Josey Morton
- (**dashboard**) add run list, inbox, and spend views - (a175e76) - Josey Morton
- (**dashboard**) build the run inspector on the real replay fold - (81f688d) - Josey Morton
- (**dashboard**) scaffold Leptos wasm app with the real replay fold - (bf445e1) - Josey Morton
- (**e2e**) seed an 8-node tool-bearing graph in e2e-serve.sh - (8b2df21) - Josey Morton
- (**engine**) execute map fan-out inline and sequentially - (31a9d71) - Josey Morton
- (**engine**) derive fork-safe idempotency keys and plan forks - (3281527) - Josey Morton
- (**engine**) execute gate and branch nodes - (70e2e72) - Josey Morton
- (**engine**) add the graph engine for linear agent and tool graphs - (6800e89) - Josey Morton
- (**examples**) complete the client-run model step offline - (9e69ed1) - Josey Morton
- (**graph**) add the fold node to the format, validator, and builders - (b0f94be) - Josey Morton
- (**graph**) add optional node display name - (c0649b3) - Josey Morton
- (**graph**) resolve a value reference with the expression path grammar - (9c35e76) - Josey Morton
- (**graph**) add branch model-decision agent hash - (ab1554b) - Josey Morton
- (**graph**) add the branch condition expression language - (0fd044a) - Josey Morton
- (**graph**) add typed graph builders for Rust, TypeScript, and Python - (ca81e90) - Josey Morton
- (**graph**) add the graph document format, validation, and CLI - (9197ece) - Josey Morton
- (**inbox**) put the run's recent substance at the gate decision point - (d3f6e78) - Josey Morton
- (**inbox**) waiting-run cards with schema-generated forms, honest budget floor and exclusive reconciliation - (b36f98e) - Josey Morton
- (**inspector**) timeline, wasm scrubber, JSON highlighting, live ticker - (0410d79) - Josey Morton
- (**llm**) allow the system prompt to be a text-block array - (8238230) - Josey Morton
- (**llm**) add streaming responses and image and document input blocks - (d40c244) - Josey Morton
- (**llm**) support OAuth bearer authentication - (d6fb4b6) - Josey Morton
- (**llm**) add the Messages API client extracted from cargo-mentor - (f19ca77) - Josey Morton
- (**replay**) record fold iteration markers and project their loop state - (7f7201a) - Josey Morton
- (**replay**) record map fan-out markers through the cursor - (68dfb8d) - Josey Morton
- (**replay**) record branch and skip graph markers - (7cfa7a6) - Josey Morton
- (**replay**) add graph-run events and per-node fold projection - (35289be) - Josey Morton
- (**replay-wasm**) fold event logs to state in the browser - (4023c30) - Josey Morton
- (**runs**) lead the view with a minute-one teaching strip - (b11b00b) - Josey Morton
- (**runtime**) expose map fan-out markers on RunCtx - (702c87a) - Josey Morton
- (**runtime**) thread run labels through Agent, Runtime, and RunCtx - (30f866f) - Josey Morton
- (**runtime**) add a streaming model call that records identically - (cc6e481) - Josey Morton
- (**runtime**) opt-in recording of model request bodies - (5dd21a6) - Josey Morton
- (**runtime**) add RunCtx, Agent builder, budgets, and the built-in loop - (33d3464) - Josey Morton
- (**sdk-py**) accept and decode run labels - (1a7798a) - Josey Morton
- (**sdk-ts**) accept and decode run labels - (04352a1) - Josey Morton
- (**sdks**) client-driven run drivers in the Python and TS SDKs - (bcc81ad) - Josey Morton
- (**sdks**) add thin Python and TypeScript control-plane clients - (564558b) - Josey Morton
- (**server**) report per-run driver liveness on the run list - (4831dfd) - Josey Morton
- (**server**) name the exact build in GET /v1/capabilities - (316c4cc) - Josey Morton
- (**server**) add fork with refuse-then-record and a capabilities probe - (c034ee2) - Josey Morton
- (**server**) add the graph control plane and graph-run execution - (0f241b9) - Josey Morton
- (**server**) accept and surface run labels over HTTP - (866c398) - Josey Morton
- (**server**) embed and serve the dashboard behind a ui feature - (32142e4) - Josey Morton
- (**server**) enrich GET /v1/runs with usage, step_count, agent_def_hash - (dcd233e) - Josey Morton
- (**server**) server-performed tool step and client-driven resolve - (3238bb0) - Josey Morton
- (**server**) server-performed model step for client-driven runs - (216580f) - Josey Morton
- (**server**) client-driven run surface over a pure append-guard validator - (c4b8258) - Josey Morton
- (**server**) add the HTTP and SSE control plane - (1094ba3) - Josey Morton
- (**store**) add the EventStore conformance kit - (b3332f3) - Josey Morton
- (**store**) add EventStore trait and SQLite backend - (d62da58) - Josey Morton
- (**tools**) add HTTP transport for remote MCP servers - (dec03b1) - Josey Morton
- (**tools**) add MCP tool integration via rmcp over stdio - (3c56513) - Josey Morton
- (**tools**) add the derive(Tool) macro - (d93e40d) - Josey Morton
- (**tools**) add typed tool contracts, erased dispatch, and registry - (27c5d0e) - Josey Morton
- (**wasm**) sandbox untrusted tools with wasmtime components - (6881573) - Josey Morton
- (**workflows**) the duplicate-id stack names itself on the canvas - (62c06e0) - Josey Morton
- add the abandon-run affordance to the Bridge - (06afc84) - Josey Morton
- add the RunAbandoned terminal event and the abandon operation - (dc05a42) - Josey Morton
- stream live progress, add salvor resolve, settle the budget type - (9dd6b47) - Josey Morton
- record the kill -9 demo and lead the readme with it - (33f95a8) - Josey Morton
- add the release-gate property suite and the demo research agent - (2d1fd00) - Josey Morton
#### Bug Fixes
- (**bridge**) pin the budget card's secondary abandon receipt against reclassification - (e5ab336) - Josey Morton
- (**bridge**) stage the abandon card's fold-away so it reads smooth, not jagged - (45e58e5) - Josey Morton
- (**bridge**) keep the stalled card's action row stable when abandon opens - (146801f) - Josey Morton
- (**bridge**) align the stalled card's action row with its siblings - (dca6183) - Josey Morton
- (**bridge**) redraw the runs status pill's filter glyph as a funnel - (34f1ddc) - Josey Morton
- (**bridge**) unify the row click target and mark the status pill as a filter - (be290bb) - Josey Morton
- (**bridge**) stop the health strip reading all-clear while runs need attention - (e77a74a) - Josey Morton
- (**bridge**) name the status groups, define client-driven runs, retitle the canvas Run mode - (833305b) - Josey Morton
- (**bridge**) give the reconciliation refusal an honest off-ramp - (e1a86b1) - Josey Morton
- (**bridge**) land the inbox signpost focus and drop the lingering signpost - (fac4342) - Josey Morton
- (**bridge**) make the held scrubber read as a preview of past state - (7e2ae12) - Josey Morton
- (**bridge**) keyboard reach for the Runs row and the Inspector scrubber - (cb22882) - Josey Morton
- (**bridge**) state a run's true resting state and fill the graph-run agent card - (f2601ad) - Josey Morton
- (**bridge**) render honest timeline detail instead of literal undefined - (5344b62) - Josey Morton
- (**bridge**) cold-load path deep links land on their view - (8015009) - Josey Morton
- (**bridge**) live-target corrections for the un-held canvas - (05cb0e4) - Josey Morton
- (**bridge**) render graph runs distinctly in the Runs agent column - (5b01ec6) - Josey Morton
- (**bridge**) keep every view within a 390px viewport - (b5fa300) - Josey Morton
- (**bridge**) close the a11y gaps the full sweep turned up - (ab7a93e) - Josey Morton
- (**bridge**) serve wasm with its real MIME type in the e2e harness - (ab7814a) - Josey Morton
- (**bridge**) render-safe focus hand-off for the panel dock - (ae2e1b8) - Josey Morton
- (**inbox**) correct the client-run enumeration caption - (94c480c) - Josey Morton
- (**inspector**) keep the event strip's ticks inside the container - (27c4ff8) - Josey Morton
- (**runs**) stall on the in-progress family, not literal running - (1182864) - Josey Morton
- (**sdks**) skip the python driver tests cleanly when httpx is absent - (1f733ce) - Josey Morton
- (**server**) stamp client-run append recorded_at with the server clock - (00033b2) - Josey Morton
- (**spend**) partition the activity histogram's click-target geometry - (f3832f5) - Josey Morton
- (**spend**) exclude zero-call runs from the all-unpriced check - (b4ccac2) - Josey Morton
- (**spend**) lead with one unpriced banner instead of scattered essays - (c1e8362) - Josey Morton
- (**spend**) exclude implausible recorded_at stamps from the activity window - (8f67206) - Josey Morton
- (**spend**) iterate the activity histogram extent so a wide event window cannot overflow the stack - (2971488) - Josey Morton
- (**workflows**) route graph edges clear of the node cards - (71d55f0) - Josey Morton
- (**workflows**) keep the node error mark off the effect badge - (64aad08) - Josey Morton
- (**workflows**) bring the canvas up to the prototype design - (f9ff504) - Josey Morton
- (**workflows**) port the canvas layout sidecar and orthogonal edge geometry - (a8387ba) - Josey Morton
#### Documentation
- (**examples**) add five examples covering the free and durable paths - (8959ab3) - Josey Morton
- (**runtime**) add runnable library examples for both tiers - (6a15eda) - Josey Morton
- (**server**) correct build.rs's dirty-tree wording - (808086e) - Josey Morton
- drop references to the personal teaching layer - (423f676) - Josey Morton
- open the quickstart with a gentle first run - (29a7391) - Josey Morton
- bring the README up to the current surface - (b425c49) - Josey Morton
- add the naming note and record the demo gif - (38f3a4e) - Josey Morton
- teach salvor resolve and the remote MCP url form in the examples - (4277061) - Josey Morton
- add Python and TypeScript MCP tool examples - (3b3e3f2) - Josey Morton
- add a live web-research example with real MCP servers - (cbd9dc9) - Josey Morton
- add readme with project one-liner and crate map - (5bceb9c) - Josey Morton
#### Tests
- (**bridge**) stub storage in the first receipts spec so it runs everywhere - (9c20162) - Josey Morton
- (**bridge**) harden load-sensitive inbox and agent-registry specs - (5755275) - Josey Morton
- (**bridge**) seed a stalled run in the e2e control plane - (9ff8046) - Josey Morton
- (**bridge**) seed real graph runs in e2e-serve - (61c8891) - Josey Morton
- (**bridge**) seed a named agent in e2e-serve for the Agent column - (0d3106c) - Josey Morton
- (**bridge**) seed labelled runs and register the recon agent in e2e-serve - (8583e92) - Josey Morton
- (**bridge**) seed one needs_reconciliation run in the e2e harness - (63c3dde) - Josey Morton
- (**bridge**) e2e serve harness for the Playwright suite - (eafd660) - Josey Morton
- (**cli**) update RunStarted fixtures for the new labels field - (c3b05a1) - Josey Morton
- (**engine**) pin the fold refusal before it records anything - (600562b) - Josey Morton
- (**sdks**) prove the live model step offline in both driver suites - (61519a0) - Josey Morton
- seed an abandoned run and fix the e2e teardown to exact pids - (af4cf57) - Josey Morton
#### Build system
- wire dormant cargo-dist distribution pipeline - (5ca0ab0) - Josey Morton
- replace hand-rolled commit hook with cocogitto - (6e963d2) - Josey Morton
- enforce conventional commits with a commit-msg hook - (1d5993c) - Josey Morton
- scaffold cargo workspace with five crates - (949e70f) - Josey Morton
#### Continuous Integration
- prove the dashboard embed in a dedicated ui job - (9a4fa83) - Josey Morton
- add the workspace test workflow - (7bb9478) - Josey Morton
#### Refactoring
- (**bridge**) open about and pedagogy surfaces with what things do - (aee9bed) - Josey Morton
- (**bridge**) point the moved highlighter specs at the shared module - (0383164) - Josey Morton
- (**bridge**) fold the two JSON highlighters into one shared module - (2213c18) - Josey Morton
- (**core**) extract the replay engine into the salvor-replay crate - (22cb34a) - Josey Morton
- (**harness**) serve the e2e suite from salvor serve itself - (84e4372) - Josey Morton
- (**runtime**) split drive into begin and drive_loop - (4511be3) - Josey Morton
- gather every example under the top-level examples directory - (4f07e59) - Josey Morton
#### Miscellaneous Chores
- (**bridge**) opt out of angular cli analytics for headless runs - (876aacd) - Josey Morton
- (**bridge**) consume salvor-replay-wasm as a dependency and build asset - (dc313cc) - Josey Morton
- ignore local-only artifacts and drop references to a removed demo script - (c904072) - Josey Morton
- dual-license under MIT OR Apache-2.0 - (4398dad) - Josey Morton
- remove license data while licensing is reconsidered - (2a06327) - Josey Morton
- add the salvor umbrella crate to hold the name - (5e31600) - Josey Morton
- add license files and dual-license wording - (ce42996) - Josey Morton
#### Style
- (**bridge**) make abandon's danger treatment destructive, not just serious - (3deb3f7) - Josey Morton
- (**bridge**) still all motion from the two motion tokens under reduced motion - (02a627f) - Josey Morton
- (**bridge**) comments state constraints, not provenance - (d1163cb) - Josey Morton

- - -

Changelog generated by [cocogitto](https://github.com/cocogitto/cocogitto).