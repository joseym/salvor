# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# The salvor control plane: `salvor serve` over HTTP + SSE, backed by a
# SQLite event store.
#
# UI decision: this image is API-ONLY, no embedded dashboard.
#
# `salvor serve` only answers the web UI when the binary is built with the
# `ui` feature AND bridge/dist/bridge/browser already exists on disk at
# compile time (rust-embed bakes that folder's bytes in). Producing that
# folder needs a second toolchain entirely: Node 24, wasm-pack, and the
# three-step chain the `ui` job in .github/workflows/ci.yml runs end to end
# (wasm-pack build crates/salvor-replay-wasm --target web, npm build the
# TypeScript client under sdks/typescript, then npm build the Angular app
# under bridge/). Folding all of that into this image would double its base
# layer, its build time, and its failure surface, for a container whose job
# is running the control plane, not re-proving the frontend build works --
# CI's `ui` job already owns that proof on every push.
#
# So the build stage below compiles salvor-cli with `--no-default-features`:
# no dashboard, no `fixture` test/demo binaries, just the `salvor` binary
# serving `/v1/...` and nothing at `/`. MCP-client support and the wasm
# sandbox still work -- they arrive through unconditional library
# dependencies (salvor-tools, salvor-wasm), not through this feature. A build
# that wants the dashboard in a container can extend this file with the `ui`
# job's steps, feed the resulting bridge/dist into the build stage, and swap
# the cargo build flag for `--features ui`. See docs/CONTAINER.md.
# ---------------------------------------------------------------------------

# ----- build stage -----------------------------------------------------
# rust:1.95-bookworm, not -slim: the workspace pins rust-version = "1.95"
# (Cargo.toml [workspace.package]), and the non-slim image already carries a
# C toolchain (gcc, make) through its buildpack-deps base. Two dependencies
# need a C compiler at build time even though nothing here talks to a system
# library: rusqlite's `bundled` feature compiles the sqlite3 C amalgamation
# from source, and the `ring` crate behind rustls compiles C/assembly. The
# slim image would need its own apt-get layer to get the same compiler; the
# full image already has it, and this stage is discarded entirely by the
# COPY --from below, so its extra size never reaches the published image.
FROM rust:1.95-bookworm AS builder
WORKDIR /build

# Manifest and lockfile first, then sources, so an unchanged Cargo.lock keeps
# the dependency-compilation layer cached across source-only rebuilds.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# --no-default-features drops two things: `fixture` (three test/demo-only
# binaries -- salvor-mcp-count-fixture, salvor-demo-research,
# salvor-demo-model -- that have no business in a shipped image) and `ui`
# (the embedded dashboard; see the note at the top of this file). What's left
# is exactly one binary, `salvor`, fully functional for `serve`.
RUN cargo build --release --locked -p salvor-cli --no-default-features

# The store directory, created and owned by the runtime stage's non-root user
# ahead of time. Docker seeds a volume's first mount from whatever already
# exists at that path in the image, so if this directory doesn't exist with
# the right owner before VOLUME below, an anonymous or named volume mounted
# there arrives owned by root and the non-root process below can't write to
# it. The distroless runtime stage has no shell to run mkdir/chown itself,
# so it's done here and copied across.
RUN mkdir -p /data && chown 65532:65532 /data

# ----- runtime stage -----------------------------------------------------
# gcr.io/distroless/cc-debian12: the binary above is an ordinary
# glibc-dynamic build (not musl), and both rusqlite's bundled C code and
# rustls's `ring` backend link against libc/libgcc/libstdc++ even though
# they're statically compiled into the crate -- the plain distroless
# `static` image (built for musl/fully-static binaries) doesn't carry those.
# `cc-debian12` carries exactly those shared libraries and nothing else: no
# shell, no package manager, no coreutils, so there is nothing on the image
# for an attacker who reaches the process to pivot with. TLS is rustls (no
# OpenSSL), so no libssl is needed either.
#
# The `nonroot` tag is what satisfies "must not run as root": it ships a
# `nonroot` user/group already provisioned at uid/gid 65532 and sets it as
# the default USER, with no useradd step required (this image has no shell
# to run one). The explicit USER line below is redundant with that default
# but kept so the non-root guarantee is visible in this file, not only in
# the base image's own documentation.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /build/target/release/salvor /usr/local/bin/salvor
COPY --from=builder --chown=65532:65532 /data /data

USER nonroot:nonroot

# The event store. `salvor serve` opens one SQLite file here, and
# SALVOR_STORE (read via the CLI's `--store` env fallback) is what points it
# at /data instead of the default ./salvor.db in the container's ephemeral
# writable layer. A volume MUST be mounted at /data, e.g.
# `-v salvor-data:/data`: skip it and every run, resume point, and event this
# container ever records is deleted the moment the container is removed or
# recreated, which defeats the entire point of a durability runtime. See
# docs/CONTAINER.md for the full `docker run` invocation.
VOLUME ["/data"]
ENV SALVOR_STORE=/data/salvor.db

# 0.0.0.0, not the CLI's own 127.0.0.1 default: a loopback bind only accepts
# connections from inside this container's network namespace, so it would be
# unreachable through a published port (`-p 8080:8080`) from outside. 8080 is
# the port this image documents and EXPOSEs; change both together if you
# remap it.
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/salvor", "serve", "--bind", "0.0.0.0:8080"]
