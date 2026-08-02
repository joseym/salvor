"use strict";

// Downloads the platform's `salvor` binary and, wherever the OS allows it,
// replaces this package's own `bin/salvor` entry file with that binary's
// raw bytes. Once that swap has happened, npm's bin symlink resolves
// straight to the native binary: launching `salvor` execs the real process
// directly, with no Node.js relay in between. Killing that PID (SIGTERM,
// SIGKILL, or Ctrl-C) kills the real run, because there is no wrapper left
// to orphan it.
//
// The swap can only happen where the OS execs a file by its own content
// (POSIX). On Windows, npm's generated .cmd/.ps1 shim always runs
// `node <target>` itself, so bin/salvor has to stay a real Node script
// there; run() below is that fallback relay, used on Windows always and
// on POSIX only if the swap could not happen (--ignore-scripts, a failed
// postinstall, a corrupted download, etc). The relay forwards SIGINT,
// SIGTERM and SIGHUP to the child and mirrors its exit status, including
// the 128+signal convention for a signal death, everything a wrapper can do.
// SIGKILL cannot be forwarded by any wrapper process; that is exactly why the
// swap, not the relay, is the fix for `kill -9`.

const {
  existsSync,
  mkdirSync,
  rmSync,
  copyFileSync,
  chmodSync,
  statSync,
  createWriteStream,
} = require("fs");
const { join, dirname, sep } = require("path");
const { spawn, spawnSync } = require("child_process");
const { mkdtemp } = require("fs/promises");
const { tmpdir } = require("os");

const { download } = require("./download");
const { getPlatform } = require("./platforms");
const pkgMeta = require("../package.json");

const PACKAGE_ROOT = join(__dirname, "..");
const ENTRY_PATH = join(PACKAGE_ROOT, "bin", "salvor");

function fail(msg) {
  console.error(msg);
  process.exit(1);
}

class Binary {
  constructor() {
    this.platform = getPlatform();
    this.installDirectory = join(PACKAGE_ROOT, ".bin_real");
    if (!existsSync(this.installDirectory)) {
      mkdirSync(this.installDirectory, { recursive: true });
    }
  }

  get downloadUrl() {
    return `https://github.com/joseym/salvor/releases/download/v${pkgMeta.version}/${this.platform.artifactName}`;
  }

  get realBinaryPath() {
    return join(this.installDirectory, this.platform.bin);
  }

  exists() {
    return existsSync(this.realBinaryPath);
  }

  async install(suppressLogs) {
    if (this.exists()) {
      if (!suppressLogs) {
        console.error("salvor is already downloaded, skipping.");
      }
      return;
    }

    try {
      rmSync(this.installDirectory, { recursive: true, force: true });
    } catch {
      // directory may not exist yet
    }
    mkdirSync(this.installDirectory, { recursive: true });

    if (!suppressLogs) {
      console.error(`Downloading salvor from ${this.downloadUrl}`);
    }

    let res;
    try {
      res = await download(this.downloadUrl);
    } catch (e) {
      fail(`salvor: error fetching release: ${e.message}`);
      return;
    }

    const directory = await mkdtemp(`${tmpdir()}${sep}`);
    const tempFile = join(directory, this.platform.artifactName);

    await new Promise((resolve, reject) => {
      const sink = res.pipe(createWriteStream(tempFile));
      sink.on("error", reject);
      sink.on("close", resolve);
    });

    let result;
    if (this.platform.artifactName.endsWith(".zip")) {
      if (process.platform === "win32") {
        result = spawnSync("powershell.exe", [
          "-NoProfile",
          "-NonInteractive",
          "-Command",
          `& { param([string]$LiteralPath, [string]$DestinationPath) Expand-Archive -LiteralPath $LiteralPath -DestinationPath $DestinationPath -Force }`,
          tempFile,
          this.installDirectory,
        ]);
      } else {
        result = spawnSync("unzip", ["-q", tempFile, "-d", this.installDirectory]);
      }
    } else {
      // tar.xz / tar.gz, stored with one leading directory component
      result = spawnSync("tar", [
        "xf",
        tempFile,
        "--strip-components",
        "1",
        "-C",
        this.installDirectory,
      ]);
    }

    if (result.error) {
      fail(`salvor: error unpacking release: ${result.error.message}`);
      return;
    }
    if (result.status !== 0) {
      fail(
        `salvor: error unpacking release: stdout: ${result.stdout}; stderr: ${result.stderr}`,
      );
      return;
    }

    if (!suppressLogs) {
      console.error("salvor has been installed!");
    }
  }

  // Overwrites bin/salvor's bytes with the downloaded binary's bytes, so
  // npm's bin symlink (which points at that same path) resolves directly to
  // the native process. No-op (cheaply) if it looks already done. POSIX
  // only: see the module comment for why Windows can't use this.
  swapEntry() {
    if (process.platform === "win32") {
      throw new Error("direct binary linking is POSIX-only");
    }
    if (!this.exists()) {
      throw new Error("downloaded binary is missing");
    }
    const src = this.realBinaryPath;
    try {
      const srcStat = statSync(src);
      const dstStat = statSync(ENTRY_PATH);
      if (dstStat.size === srcStat.size) return; // already swapped
    } catch {
      // fall through and copy
    }
    mkdirSync(dirname(ENTRY_PATH), { recursive: true });
    copyFileSync(src, ENTRY_PATH);
    chmodSync(ENTRY_PATH, 0o755);
  }
}

function getBinary() {
  if (!pkgMeta.version) {
    console.warn("salvor: package has no version set, skipping install");
    return null;
  }
  return new Binary();
}

const SIGNAL_EXIT_CODES = { SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGTERM: 15, SIGKILL: 9 };

// The fallback relay: used when the swap above didn't happen. Downloads on
// first run if needed, then attempts the swap itself so this relay does not
// run again next time (constraint: a download-on-first-run wrapper must get
// out of the way for subsequent invocations). Then execs the real binary as
// a child, forwarding signals and exit status.
function run(binaryName) {
  const bin = getBinary();
  if (!bin) fail("salvor: no download configured for this package");

  const ready = bin.exists() ? Promise.resolve() : bin.install(true);

  ready
    .then(() => {
      if (process.platform !== "win32") {
        try {
          bin.swapEntry();
        } catch {
          // best effort; this run still goes through the relay below
        }
      }

      const args = process.argv.slice(2);
      const child = spawn(bin.realBinaryPath, args, { stdio: "inherit" });

      const signals = ["SIGINT", "SIGTERM", "SIGHUP"];
      const forwarders = new Map();
      for (const sig of signals) {
        const forward = () => child.kill(sig);
        forwarders.set(sig, forward);
        process.on(sig, forward);
      }
      const stopForwarding = () => {
        for (const [sig, forward] of forwarders) process.removeListener(sig, forward);
      };

      child.on("error", (err) => {
        stopForwarding();
        fail(`salvor: ${err.message}`);
      });
      child.on("exit", (code, signal) => {
        stopForwarding();
        if (signal) {
          process.exit(128 + (SIGNAL_EXIT_CODES[signal] || 0));
        }
        process.exit(code === null ? 1 : code);
      });
    })
    .catch((e) => fail(e.message || String(e)));
}

module.exports = { Binary, getBinary, run, ENTRY_PATH };
