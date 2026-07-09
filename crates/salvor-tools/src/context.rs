//! [`ToolCtx`]: the per-attempt context a tool's
//! [`call`](crate::ToolHandler::call) receives.

/// What the runtime hands a tool for one execution attempt.
///
/// This type is kept deliberately small. The only thing a handler clearly
/// needs is the idempotency key for the current
/// attempt: an [`Idempotent`](salvor_core::Effect::Idempotent) tool uses it so
/// that a retried attempt reuses the same key and the provider collapses the
/// duplicate into one effect. Everything else a tool might want (recorded
/// clock, randomness, model calls) belongs to `RunCtx` in the orchestration
/// layer, not to a single tool call, so it stays out of here.
///
/// The type is designed for additive growth. Fields are private and reached
/// through accessors, so run identity, the current sequence number, or a
/// tracing span can be added later without breaking a tool's `call` signature.
///
/// It is constructed by the runtime, not by tools. The runtime builds
/// one `ToolCtx` per dispatch from the idempotency key the replay cursor
/// carries for that tool call; tests construct it directly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolCtx {
    idempotency_key: Option<String>,
}

impl ToolCtx {
    /// Builds a context carrying the idempotency key for this attempt.
    ///
    /// Pass `None` for a tool call that has no key (a [`Read`], or a call the
    /// runtime chose not to key); pass `Some(key)` for the keyed retry of an
    /// [`Idempotent`](salvor_core::Effect::Idempotent) tool.
    ///
    /// [`Read`]: salvor_core::Effect::Read
    pub fn new(idempotency_key: Option<String>) -> Self {
        Self { idempotency_key }
    }

    /// The idempotency key for the current attempt, if the runtime set one.
    ///
    /// An idempotent tool that talks to a provider passes this through as the
    /// provider's idempotency key so that a retry reuses it and does not create
    /// a second effect.
    pub fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
    }
}
