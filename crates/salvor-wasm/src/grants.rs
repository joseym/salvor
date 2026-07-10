//! Directory grants: the only v0.2 capability an operator can hand a guest
//! beyond nothing.

use std::path::PathBuf;

use wasmtime_wasi::{DirPerms, FilePerms};

/// What a guest may do inside one granted directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantPerms {
    /// List and read only.
    Read,
    /// List, read, create, write, and delete.
    ReadWrite,
}

impl GrantPerms {
    /// The wasmtime-wasi permission pair this grant level maps to. WASI
    /// splits permissions between the directory handle (open/list/mutate
    /// entries) and the files reached through it (read/write bytes); one
    /// operator-facing level sets both consistently so "read" can never
    /// accidentally leave file writes open.
    pub(crate) fn wasi_perms(self) -> (DirPerms, FilePerms) {
        match self {
            GrantPerms::Read => (DirPerms::READ, FilePerms::READ),
            GrantPerms::ReadWrite => (DirPerms::all(), FilePerms::all()),
        }
    }
}

/// One preopened directory: a host path exposed to the guest at a guest path,
/// at a permission level.
///
/// This is the entire v0.2 capability surface. A guest with no grants can
/// open nothing: the WASI context is otherwise empty, so `open("/etc/hosts")`
/// inside the guest fails with a not-found error because from the guest's
/// point of view no filesystem exists outside its preopens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirGrant {
    /// The host directory being exposed. Must exist at call time; resolved by
    /// whoever constructs the grant (the CLI resolves it relative to the
    /// agent file's directory).
    pub host: PathBuf,
    /// The path the guest sees it at (for example `/data`).
    pub guest: String,
    /// What the guest may do inside it.
    pub perms: GrantPerms,
}
