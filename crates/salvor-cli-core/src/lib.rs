//! The pure half of the `salvor` command-line surface: the `clap` command
//! tree and the value-to-text renderer, with nothing that performs IO, reads
//! a clock, or opens a store.
//!
//! - [`cli`] is the `clap` parse tree: every verb, every flag, every help
//!   string, and the two [`clap::builder::TypedValueParser`] implementations
//!   behind `salvor list --status` and `--group`.
//! - [`render`] turns runtime and store values into the text the CLI prints.
//! - [`graph_editor`] is a line-oriented editor for a graph document, built as
//!   an event fold so its state is a pure function of the commands typed at
//!   it.
//!
//! The two belong together because they already agree with each other: the
//! `--status` parser offers exactly the labels [`render::status_label`] can
//! print, and the `--group` refusal names the group [`render::status_group`]
//! puts a status in.
//!
//! # Why this is a crate of its own
//!
//! `salvor-cli` is the operator surface: it opens a SQLite store, spawns
//! processes, and drives an async runtime, none of which exist in a browser.
//! Everything here is a pure function of its arguments, so it compiles to
//! `wasm32-unknown-unknown` as well as to a native binary. A terminal
//! rendered in a browser can therefore parse a typed command line and format
//! its output with THIS code, rather than a second implementation that would
//! drift from the real CLI the moment either changed.
//!
//! What stays behind in `salvor-cli` is whatever needs the host: the command
//! handlers, the store, and the pieces of rendering that describe live
//! processes.

#![warn(missing_docs)]

pub mod cli;
pub mod graph_editor;
pub mod render;
