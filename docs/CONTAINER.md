# Running salvor in a container

## What's in the image

The published image is **API-only**: it does not embed the Angular
dashboard. `salvor serve` answers `/v1/...` (the HTTP + SSE control plane)
but `/` responds with a plain-text note that the binary was built without a
UI, because building the dashboard needs a whole second toolchain (Node 24,
wasm-pack, and the Angular build) that this image's build stage deliberately
does not run. See the comment at the top of the repository root
[`Dockerfile`](../Dockerfile) for the full reasoning. If you need the
dashboard, run `salvor serve` from a checkout that has built `bridge/` (see
the main [README](../README.md)), or extend the Dockerfile yourself with the
`ui` job's steps from [`.github/workflows/ci.yml`](../.github/workflows/ci.yml).

## Running it

```sh
docker run -d \
  --name salvor \
  -p 8080:8080 \
  -v salvor-data:/data \
  ghcr.io/joseym/salvor:latest
```

- `-p 8080:8080` publishes the control plane's port. The image binds
  `0.0.0.0:8080` inside the container (not `127.0.0.1`, which would be
  unreachable through the published port).
- `-v salvor-data:/data` mounts a named volume at `/data`, where the image
  keeps its SQLite event store (`/data/salvor.db`, via the `SALVOR_STORE`
  environment variable baked into the image).

**The volume mount is not optional.** Every run, resume point, and event
this container ever records lives in that one SQLite file. Without a volume
mounted at `/data`, the store lives in the container's own writable layer,
and it is deleted the moment the container is removed or recreated (`docker
rm`, `docker run` again, a redeploy) -- silently, with no warning at
runtime. A durability runtime that loses its event log on restart isn't
durable, so treat this mount as load-bearing, not optional tuning.

To confirm the volume is doing its job: stop and remove the container, then
start a new one against the same volume, and check that `salvor list`
(against the same store, e.g. via `docker exec` or a second container
sharing the volume) still shows the runs recorded before the restart.

## Configuration

The image's `ENTRYPOINT` runs `salvor serve --bind 0.0.0.0:8080`; anything
appended to `docker run` after the image name is passed through as
additional arguments (for example, `--auth-token` to require a bearer
token -- see `salvor serve --help` for the full flag list).

- `SALVOR_STORE` is preset to `/data/salvor.db`. Override it with `-e
  SALVOR_STORE=/data/other.db` if you want a different filename inside the
  same mounted volume.
- The container runs as a fixed non-root user (uid/gid 65532, distroless's
  `nonroot`), not root.

## Building and publishing

[`.github/workflows/docker.yml`](../.github/workflows/docker.yml) builds the
image for `linux/amd64` on pushes to `main` that touch something the image is
built from (the `Dockerfile`, `.dockerignore`, `Cargo.toml`, `Cargo.lock`, or
`crates/`), proving the Dockerfile still compiles without publishing
anything. Commits that cannot affect the image — docs, the Bridge, the SDKs —
skip it, so the answer arrives quickly when it is actually needed. A `v*` tag
builds both `linux/amd64` and `linux/arm64`, regardless of what it touched,
and pushes them to `ghcr.io/joseym/salvor`.

The published images are multi-architecture, so `docker run` picks the right
one for your machine. Only the push-to-main proof is single-arch: the arm64
leg has no native runner, so it builds under emulation, and re-proving the
same Dockerfile that slowly on every commit costs feedback time and a
concurrency slot the Rust gates want.

Tagging on a release:

- a stable version (`v1.2.3`) publishes `1.2.3` **and** moves `latest`;
- a prerelease (`v1.2.3-rc.1`) publishes `1.2.3-rc.1` **only**, leaving
  `latest` on the last stable release, so pulling `:latest` never lands you
  on a release candidate by accident.

It uses the repository's built-in `GITHUB_TOKEN` (granted `packages: write`);
no separate registry credential is required. This is a distinct pipeline from
the binary releases described in [RELEASING.md](RELEASING.md), which `dist`
builds and publishes on the same tag push.
