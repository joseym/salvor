//! Integration tests driving the real host against the real fixture guest:
//! the component at `tests/fixture-guest/` is built (once, release, for
//! wasm32-wasip2) and every scenario runs through the public [`DynTool`]
//! surface, exactly as the runtime loop would dispatch it.
//!
//! The suite needs the `wasm32-wasip2` rustup target; the build helper says
//! so loudly when it is missing. Everything else is hermetic: no network at
//! test time (the guest's dependencies come from the local cargo cache once
//! fetched), no external binaries beyond cargo itself.
//!
//! One test is `#[ignore]`d by default: `external_component_proof`, the
//! harness used to prove non-Rust guests (componentize-py, jco) against the
//! same host. Point `SALVOR_WASM_COMPONENT` at any component implementing the
//! fixture's `wordcount`/`fail`/`spin` modes and run it with `-- --ignored`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use salvor_core::Effect;
use salvor_tools::{
    DynTool, HandlerError, IdempotencyPath, ToolCtx, ToolError, ToolHandler, ToolMeta, ToolOutcome,
    ToolSet,
};
use salvor_wasm::{
    DirGrant, GrantPerms, LimitExceeded, LimitKind, ToolLimits, WasmEngine, WasmError, WasmTool,
    WasmToolSpec,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Builds the fixture guest once per test-binary run and returns the
/// component path. Serialized by `OnceLock`; a second call reuses the build.
fn fixture_component() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let guest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture-guest");
        // Build output goes to the workspace root's target/ (not the guest
        // crate's own directory): the guest is excluded from the workspace,
        // so cargo would otherwise nest a second target/ full of generated
        // files inside crates/, which tooling that sweeps the source trees
        // would then trip over.
        let target_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/wasm-guests");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--target", "wasm32-wasip2", "--release", "--quiet"])
            .arg("--target-dir")
            .arg(&target_dir)
            .current_dir(&guest_dir)
            .status()
            .expect("spawning cargo to build the fixture guest");
        assert!(
            status.success(),
            "building the fixture guest failed; if the target is missing, run \
             `rustup target add wasm32-wasip2`"
        );
        target_dir.join("wasm32-wasip2/release/fixture_guest.wasm")
    })
}

/// An operator spec with test defaults: Read effect, default limits, no
/// grants. Tests override what they exercise.
fn base_spec() -> WasmToolSpec {
    WasmToolSpec {
        name: "fixture".to_owned(),
        description: "the misbehaving test guest".to_owned(),
        effect: Effect::Read,
        input_schema: json!({ "type": "object" }),
        idempotency_key: None,
        limits: ToolLimits::default(),
        grants: Vec::new(),
    }
}

/// Loads the fixture component behind a fresh engine with the given spec.
fn load_fixture(spec: WasmToolSpec) -> WasmTool {
    let engine = WasmEngine::new().expect("engine builds");
    WasmTool::load(engine, fixture_component(), None, spec).expect("fixture component loads")
}

async fn call(tool: &dyn DynTool, input: Value) -> Result<ToolOutcome<Value>, ToolError> {
    tool.call_json(&ToolCtx::default(), input).await
}

/// Unwraps a successful output value, panicking on suspension or error.
async fn call_output(tool: &dyn DynTool, input: Value) -> Value {
    match call(tool, input).await.expect("call succeeds") {
        ToolOutcome::Output(value) => value,
        ToolOutcome::Suspend(_) => panic!("wasm tools cannot suspend"),
    }
}

/// Walks a [`ToolError`]'s source chain looking for the typed limit error.
/// This is the routing contract the crate documents: limit traps are
/// `Handler` errors whose chain carries a downcastable [`LimitExceeded`].
fn limit_in_chain(error: &ToolError) -> Option<LimitExceeded> {
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if let Some(limit) = current.downcast_ref::<LimitExceeded>() {
            return Some(*limit);
        }
        source = current.source();
    }
    None
}

/// JSON goes in as a `Value`, crosses the boundary as a string, and comes
/// back structurally identical, nested unicode and all.
#[tokio::test]
async fn json_round_trips_through_dyn_tool() {
    let tool = load_fixture(base_spec());
    let payload = json!({
        "text": "héllo wörld ✓",
        "nested": { "numbers": [1, 2.5, -3], "flag": true, "nothing": null },
    });
    let output = call_output(&tool, json!({ "mode": "echo", "value": payload })).await;
    assert_eq!(output, json!({ "echo": payload }));

    let counted = call_output(
        &tool,
        json!({ "mode": "wordcount", "text": "one two three" }),
    )
    .await;
    assert_eq!(counted, json!({ "words": 3, "chars": 13 }));
}

/// The descriptor the model sees is the operator's spec, verbatim: the guest
/// was never asked.
#[tokio::test]
async fn descriptor_is_operator_declared() {
    let tool = load_fixture(base_spec());
    let descriptor = tool.descriptor();
    assert_eq!(descriptor.name, "fixture");
    assert_eq!(descriptor.description, "the misbehaving test guest");
    assert_eq!(descriptor.effect, Effect::Read);
    assert_eq!(descriptor.input_schema, json!({ "type": "object" }));
}

/// The same component under two specs carries two different effects: effect
/// is configuration, not guest behavior.
#[tokio::test]
async fn effect_comes_from_config_not_guest() {
    let read_tool = load_fixture(base_spec());
    let mut write_spec = base_spec();
    write_spec.effect = Effect::Write;
    let write_tool = load_fixture(write_spec);
    assert_eq!(read_tool.effect(), Effect::Read);
    assert_eq!(write_tool.effect(), Effect::Write);
}

/// A guest-returned error (the WIT result's err side) is a handler failure
/// carrying the guest's message, with no limit error in the chain.
#[tokio::test]
async fn guest_error_maps_to_handler() {
    let tool = load_fixture(base_spec());
    let error = call(
        &tool,
        json!({ "mode": "fail", "message": "bad input shape" }),
    )
    .await
    .expect_err("fail mode errors");
    match &error {
        ToolError::Handler { tool, .. } => assert_eq!(tool, "fixture"),
        other => panic!("expected Handler, got {other:?}"),
    }
    let chain = full_chain(&error);
    assert!(chain.contains("guest failure: bad input shape"), "{chain}");
    assert!(limit_in_chain(&error).is_none());
}

/// A runaway loop dies at the epoch deadline, promptly, and the error is the
/// typed wall-time limit, not a mystery trap.
#[tokio::test]
async fn runaway_loop_hits_wall_time_cap() {
    let mut spec = base_spec();
    spec.limits.wall_time_ms = 200;
    let tool = load_fixture(spec);
    let started = Instant::now();
    let error = call(&tool, json!({ "mode": "spin" }))
        .await
        .expect_err("spin must be killed");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "deadline enforcement took {elapsed:?}"
    );
    let limit = limit_in_chain(&error).expect("limit error in chain");
    assert_eq!(limit.kind, LimitKind::WallTime);
    assert_eq!(limit.limit, 200);
    assert!(full_chain(&error).contains("wall-time limit of 200 ms"));
}

/// An allocation bomb is denied at the memory cap and the error names the
/// cap in bytes, instead of surfacing the allocator's opaque `unreachable`.
#[tokio::test]
async fn allocation_bomb_hits_memory_cap() {
    let mut spec = base_spec();
    spec.limits.memory_bytes = 32 * 1024 * 1024;
    let tool = load_fixture(spec);
    let error = call(&tool, json!({ "mode": "alloc" }))
        .await
        .expect_err("alloc must be denied");
    let limit = limit_in_chain(&error).expect("limit error in chain");
    assert_eq!(limit.kind, LimitKind::Memory);
    assert_eq!(limit.limit, 32 * 1024 * 1024);
    assert!(full_chain(&error).contains("33554432 bytes"));
}

/// With a tiny fuel budget armed, even the well-behaved word count runs out
/// of deterministic fuel.
#[tokio::test]
async fn fuel_budget_traps_when_exhausted() {
    let mut spec = base_spec();
    spec.limits.fuel = Some(1_000);
    let tool = load_fixture(spec);
    let error = call(&tool, json!({ "mode": "wordcount", "text": "a b c" }))
        .await
        .expect_err("fuel must run out");
    let limit = limit_in_chain(&error).expect("limit error in chain");
    assert_eq!(limit.kind, LimitKind::Fuel);
    assert_eq!(limit.limit, 1_000);
}

/// A correct sha256 pin loads; a wrong one refuses before instantiation.
#[tokio::test]
async fn sha256_pin_gates_loading() {
    let bytes = std::fs::read(fixture_component()).expect("component bytes");
    let good_pin = format!("{:x}", Sha256::digest(&bytes));

    let engine = WasmEngine::new().expect("engine builds");
    // Uppercase hex must also match: pins are compared case-insensitively.
    WasmTool::load(
        Arc::clone(&engine),
        fixture_component(),
        Some(&good_pin.to_uppercase()),
        base_spec(),
    )
    .expect("correct pin loads");

    let bad_pin = "00".repeat(32);
    match WasmTool::load(engine, fixture_component(), Some(&bad_pin), base_spec()) {
        Err(WasmError::Sha256Mismatch {
            expected, actual, ..
        }) => {
            assert_eq!(expected, bad_pin);
            assert_eq!(actual, good_pin);
        }
        Err(other) => panic!("expected Sha256Mismatch, got {other}"),
        Ok(_) => panic!("wrong pin must refuse to load"),
    }
}

/// With no grants the guest has no filesystem at all: a probe for a real
/// host file fails inside the guest.
#[tokio::test]
async fn no_grants_means_no_filesystem() {
    let tool = load_fixture(base_spec());
    let error = call(&tool, json!({ "mode": "read_file", "path": "/etc/hosts" }))
        .await
        .expect_err("no preopen, no read");
    // The failure is the guest's own io error (Handler), not a host crash,
    // and certainly not file content.
    assert!(matches!(error, ToolError::Handler { .. }));
    assert!(full_chain(&error).contains("reading `/etc/hosts`"));
}

/// A read grant exposes exactly the granted tree: reads inside it work,
/// writes inside it are refused, paths outside it stay invisible.
#[tokio::test]
async fn read_grant_allows_reads_and_nothing_else() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("hello.txt"), "from the host").expect("seed file");

    let mut spec = base_spec();
    spec.grants = vec![DirGrant {
        host: dir.path().to_owned(),
        guest: "/data".to_owned(),
        perms: GrantPerms::Read,
    }];
    let tool = load_fixture(spec);

    let output = call_output(
        &tool,
        json!({ "mode": "read_file", "path": "/data/hello.txt" }),
    )
    .await;
    assert_eq!(output, json!({ "content": "from the host" }));

    let error = call(
        &tool,
        json!({ "mode": "write_file", "path": "/data/out.txt", "content": "nope" }),
    )
    .await
    .expect_err("read grant must not allow writes");
    assert!(full_chain(&error).contains("writing `/data/out.txt`"));

    let error = call(&tool, json!({ "mode": "read_file", "path": "/etc/hosts" }))
        .await
        .expect_err("ungranted paths stay invisible");
    assert!(matches!(error, ToolError::Handler { .. }));
}

/// A readwrite grant lets the guest create a file the host then observes.
#[tokio::test]
async fn readwrite_grant_allows_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut spec = base_spec();
    spec.grants = vec![DirGrant {
        host: dir.path().to_owned(),
        guest: "/scratch".to_owned(),
        perms: GrantPerms::ReadWrite,
    }];
    let tool = load_fixture(spec);

    let output = call_output(
        &tool,
        json!({ "mode": "write_file", "path": "/scratch/note.txt", "content": "guest wrote this" }),
    )
    .await;
    assert_eq!(output, json!({ "written": 16 }));
    let on_host = std::fs::read_to_string(dir.path().join("note.txt")).expect("host reads it back");
    assert_eq!(on_host, "guest wrote this");
}

/// A grant whose host directory does not exist fails as a clear setup error,
/// not a guest crash.
#[tokio::test]
async fn missing_grant_directory_is_a_clear_error() {
    let mut spec = base_spec();
    spec.grants = vec![DirGrant {
        host: PathBuf::from("/nonexistent-salvor-grant"),
        guest: "/data".to_owned(),
        perms: GrantPerms::Read,
    }];
    let tool = load_fixture(spec);
    let error = call(&tool, json!({ "mode": "wordcount", "text": "hi" }))
        .await
        .expect_err("missing host dir must fail");
    assert!(full_chain(&error).contains("preopening host directory"));
}

/// A minimal native tool for the registry test below.
struct Upper;

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct UpperInput {
    text: String,
}

#[derive(serde::Serialize)]
struct UpperOutput {
    upper: String,
}

impl ToolMeta for Upper {
    const NAME: &'static str = "upper";
    const DESCRIPTION: &'static str = "uppercases text";
    const EFFECT: Effect = Effect::Read;
}

#[async_trait::async_trait]
impl ToolHandler for Upper {
    type Input = UpperInput;
    type Output = UpperOutput;

    async fn call(
        &self,
        _ctx: &ToolCtx,
        input: UpperInput,
    ) -> Result<ToolOutcome<UpperOutput>, HandlerError> {
        Ok(ToolOutcome::Output(UpperOutput {
            upper: input.text.to_uppercase(),
        }))
    }
}

/// A wasm tool registers in a [`ToolSet`] beside a native tool and both
/// dispatch through the same erased seam. (MCP tools join the same registry
/// through the same `register_dyn` path; the CLI's config tests cover that
/// combination.)
#[tokio::test]
async fn wasm_tool_registers_beside_native_tools() {
    let mut tools = ToolSet::new();
    tools.register(Upper).expect("native registers");
    tools
        .register_dyn(Box::new(load_fixture(base_spec())))
        .expect("wasm registers");

    let names: Vec<String> = tools
        .descriptors()
        .into_iter()
        .map(|descriptor| descriptor.name)
        .collect();
    assert_eq!(names, vec!["fixture".to_owned(), "upper".to_owned()]);

    let native = tools.get("upper").expect("native tool present");
    let output = call_output(native, json!({ "text": "hi" })).await;
    assert_eq!(output, json!({ "upper": "HI" }));

    let sandboxed = tools.get("fixture").expect("wasm tool present");
    let output = call_output(sandboxed, json!({ "mode": "wordcount", "text": "a b" })).await;
    assert_eq!(output, json!({ "words": 2, "chars": 3 }));
}

/// The polyglot proof harness: runs the standard scenario set against ANY
/// component implementing the fixture's `wordcount`/`fail`/`spin` modes.
/// Ignored by default because the component comes from outside the repo (a
/// componentize-py or jco build); see `examples/wasm-tools/README.md` for
/// the guest recipes.
///
/// ```text
/// SALVOR_WASM_COMPONENT=/path/to/guest.wasm \
///   cargo test -p salvor-wasm --test wasm_tool -- --ignored external_component_proof
/// ```
#[tokio::test]
#[ignore = "needs SALVOR_WASM_COMPONENT pointing at an externally built component"]
async fn external_component_proof() {
    let path = PathBuf::from(
        std::env::var("SALVOR_WASM_COMPONENT")
            .expect("set SALVOR_WASM_COMPONENT to the component under proof"),
    );
    let engine = WasmEngine::new().expect("engine builds");
    let mut spec = base_spec();
    spec.name = "external".to_owned();
    // Componentized-Python guests embed CPython and want real memory; the
    // default 128 MiB cap holds. Wall time stays tight enough to prove the
    // trap quickly.
    spec.limits.wall_time_ms = 2_000;
    let tool = WasmTool::load(engine, &path, None, spec).expect("external component loads");

    // Proof 1: the JSON round trip, unicode included.
    let output = call_output(
        &tool,
        json!({ "mode": "wordcount", "text": "polyglot guests ✓ still count wörds" }),
    )
    .await;
    assert_eq!(output, json!({ "words": 6, "chars": 35 }));

    // Proof 2: a guest-level error crosses as an error, not a crash.
    let error = call(&tool, json!({ "mode": "fail", "message": "proof" }))
        .await
        .expect_err("fail mode errors");
    assert!(matches!(error, ToolError::Handler { .. }));
    assert!(limit_in_chain(&error).is_none());

    // Proof 3: the sandbox's wall-time cap kills a runaway loop in this
    // language too.
    let started = Instant::now();
    let error = call(&tool, json!({ "mode": "spin" }))
        .await
        .expect_err("spin must be killed");
    let limit = limit_in_chain(&error).expect("limit error in chain");
    assert_eq!(limit.kind, LimitKind::WallTime);
    assert!(started.elapsed() < Duration::from_secs(10));
    println!(
        "external component proof passed for {} in {:?}",
        path.display(),
        started.elapsed()
    );
}

/// A wasm tool the operator keyed derives its idempotency key from the named
/// input field, in the same `<tool>:<value>` form an MCP tool does. The guest
/// has no say in it: nothing crosses the boundary to ask.
#[tokio::test]
async fn a_declared_key_is_derived_from_the_named_field() {
    let mut spec = base_spec();
    spec.effect = Effect::Write;
    spec.idempotency_key = Some(IdempotencyPath::parse("claim_id").expect("path parses"));
    let tool = load_fixture(spec);

    assert_eq!(
        tool.idempotency_key(&json!({"mode": "echo", "claim_id": "wreck-9931"})),
        Some("fixture:wreck-9931".to_owned())
    );
    // A tool with no declaration is keyless, exactly as before.
    assert_eq!(
        load_fixture(base_spec()).idempotency_key(&json!({"claim_id": "wreck-9931"})),
        None
    );
}

/// A keyed wasm tool called without the field it is keyed on is refused before
/// the sandbox is entered, and the refusal names the tool, the path, and the
/// keys the input carried.
///
/// The `fail` mode is the proof that the guest never ran: had it been
/// instantiated it would have returned its own handler error instead.
#[tokio::test]
async fn a_call_missing_the_declared_field_never_reaches_the_guest() {
    let mut spec = base_spec();
    spec.effect = Effect::Write;
    spec.idempotency_key = Some(IdempotencyPath::parse("claim_id").expect("path parses"));
    let tool = load_fixture(spec);

    let error = call(&tool, json!({ "mode": "fail", "message": "guest ran" }))
        .await
        .expect_err("a keyed tool refuses an input with no key");
    match &error {
        ToolError::MissingIdempotencyKey { tool, path, detail } => {
            assert_eq!(tool, "fixture");
            assert_eq!(path, "claim_id");
            // Sorted, because that is the order a JSON object's keys come back
            // in; the message is the same whichever order the caller wrote them.
            assert!(
                detail.contains("message, mode"),
                "names the keys present: {detail}"
            );
        }
        other => panic!("expected MissingIdempotencyKey, got {other:?}"),
    }
    assert!(
        !full_chain(&error).contains("guest failure"),
        "the guest must not have run: {}",
        full_chain(&error)
    );
}

/// The full rendered error chain, the same shape the runtime records.
fn full_chain(error: &ToolError) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        message.push_str(": ");
        message.push_str(&current.to_string());
        source = current.source();
    }
    message
}
