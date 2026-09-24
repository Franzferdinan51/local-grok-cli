<div align="center">

<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://media.x.ai/v1/website/spacexai-symbol-white-transparent-0c31957f.png">
    <source media="(prefers-color-scheme: light)" srcset="https://media.x.ai/v1/website/spacexai-symbol-black-transparent-6435cf42.png">
    <img alt="SpaceXAI logo" src="https://media.x.ai/v1/website/spacexai-symbol-black-transparent-6435cf42.png" width="96">
  </picture>
  <br>
  Grok Local (<code>grok-local</code>)
</h1>

**Grok Local** is an offline-first fork of SpaceXAI's terminal-based AI coding
agent. It runs as a full-screen TUI that understands your codebase, edits files,
executes shell commands, searches the web via a local SearxNG instance, and
manages long-running tasks — without requiring xAI servers at startup.

[Installing the released binary](#installing-the-released-binary) ·
[Building from source](#building-from-source) ·
[Documentation](#documentation) ·
[Repository layout](#repository-layout) ·
[Development](#development) ·
[Contributing](#contributing) ·
[License](#license)

![Grok Build TUI](https://media.x.ai/v1/website/universe-tui-screenshot-6f7a0837.png)

**Learn more about Grok Build at [x.ai/cli](https://x.ai/cli)**

This tree is the `grok-local` fork. Config lives in `~/.grok-local` (override
with `$GROK_HOME`) so it does not collide with an official `grok` install.
Inference defaults to **LM Studio** at `http://127.0.0.1:1234/v1`. The model
picker lists chat models LM Studio exposes (`GET /api/v0/models`, then
`/v1/models`): loaded llms/vlms first, then the rest of the downloaded library.
Embeddings stay hidden. Set `LM_STUDIO_MODEL` to pick one by id. Web search
defaults to SearxNG at `http://127.0.0.1:8888` (`SEARXNG_URL` or
`GROK_SEARXNG_URL`; a trailing `/search` is stripped). Do not point this at
Open WebUI on `:8080`. To reach a SearxNG server on another machine on your
tailnet:

```sh
export SEARXNG_URL=https://your-tailnet-host.ts.net/searxng
```

If your LM Studio server requires authentication, set the `LM_STUDIO_API_KEY`
environment variable to your Bearer token. This is only needed when your
LM Studio server has API-token auth enabled. When unset or blank, the
client uses an unauthenticated default (`lm-studio`) that works with
unprotected LM Studio instances:

```sh
export LM_STUDIO_API_KEY="your-token-here"
```

A small `SOURCE_REV` file at the root records the grok-build monorepo SHA
in this tree. `grok-local --version` prints **both** the grok-local fork
version and the grok-build version that overlay was applied on.

```sh
grok-local --version
# grok-local 0.5.2 (<git sha>)
# grok-build 1.0.38 (<SOURCE_REV>)
```

Two update paths — official `grok` from x.ai is never installed:

```sh
# Install the latest grok-local binary from our GitHub Releases
grok-local update --check
grok-local update

# Overlay-merge latest https://github.com/xai-org/grok-build into this tree
# (keeps LM Studio, SearxNG, ~/.grok-local, grok-local name)
grok-local update --upstream --check
grok-local update --upstream
# or: python3 scripts/sync-upstream.py
```

Do not run official `grok update` or `https://x.ai/cli/install.sh` against this
tree; those install official `grok` and would overwrite this fork.

</div>

---

## Installing

This fork is **not** the official `grok` CLI. Do not pipe `https://x.ai/cli/install.sh`
(or `install.ps1`) into a shell — those installers put official `grok` under
`~/.grok` and will collide with this tree's `~/.grok-local` layout.

Build from source on **Linux, macOS, and Windows**:

```sh
cargo build -p xai-grok-pager-bin --release
./target/release/grok-local --version          # Windows: target\release\grok-local.exe
```

Install the built Unix binary without replacing the official `grok` command:

```sh
./scripts/install.sh
# or: GROK_LOCAL_INSTALL_DIR=/some/bin ./scripts/install.sh
```

The installer writes only `grok-local` (by default to `~/.local/bin`) and never
creates, replaces, or redirects `grok`. Put `target/release` (or a copy of
`grok-local`) on your `PATH` manually on Windows. Config still lives in
`~/.grok-local` (`%USERPROFILE%\.grok-local` on Windows), override with
`$GROK_HOME`.

On Windows PowerShell, install the built binary and add the user bin directory
to `PATH` with:

```powershell
.\scripts\install.ps1
```

Use `-BinaryPath` to install a binary from another location, or `-InstallDir`
to choose a different user bin directory.

## Minimum requirements

For running the **prebuilt binary** from GitHub Releases. (Build-time
requirements — Rust, DotSlash, protoc — are in the next section.)

- **OS / CPU** — 64-bit only, and only the platforms CI ships:
  Linux x86_64, macOS on Apple Silicon, Windows x86_64. There are no
  official builds for Intel macOS, ARM Windows, ARM Linux, or 32-bit.
- **Disk** — ~250 MB for the binary (release assets measure 155–230 MB
  by platform), plus ~1 GB for the built-in SystemOne router (torch
  ~0.6 GB, GLiClass edge checkpoint 256 MB — both measured), plus
  ~15 GB if you enable the Jeff-1 second head (its model weights in the
  HuggingFace cache — measured; skip with `SYSTEMONE_JEFF1=0`).
- **Inference (the real requirement)** — grok-local needs somewhere to
  run models: LM Studio locally (OpenAI-compatible,
  `http://localhost:1234` by default) or an API key for a hosted
  provider. Model requirements are the *model's*, not the tool's: a
  35B-class local model wants tens of GB of RAM/VRAM — check the model
  card / LM Studio for the specific model before loading it. Reference
  setup: Windows 11 PC with 64 GB RAM serving models through LM Studio;
  Mac mini (M4 Pro, 24 GB) reaching them over LM Link.
- **SystemOne routing (on by default)** — the router is a Python shim,
  so it needs Python >= 3.10 (per the shim's `requires-python`).
  grok-local looks for `python3.11` by default; override with
  `SYSTEMONE_PYTHON`. Auto-start is attempted on Unix only — on Windows
  start the shim yourself (`python -m systemone.shim`) or keep it
  running; grok-local probes port `:8765` either way and works fail-open
  without it. Kill switches: `GROK_LOCAL_SYSTEMONE=0` (routing off),
  `SYSTEMONE_JEFF1=0` (GLiClass only, no Jeff-1 second head).

## Building from source

Requirements:

- **Rust** — the toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml);
  `rustup` installs it automatically on first build. On a Mac, rustup adds the
  **host** triple: `aarch64-apple-darwin` on Apple Silicon (M1, M2, M3, M4, M5
  including Pro / Max / Ultra — there is no separate M5 target) or
  `x86_64-apple-darwin` on Intel. Extra Linux triples in that file are for
  Linux CI only.
- **[DotSlash](https://dotslash-cli.com)** — required so hermetic tools under
  [`bin/`](bin/) (notably [`bin/protoc`](bin/protoc)) can download and run.
  Install it and ensure `dotslash` is on your `PATH` **before** building:

  ```sh
  cargo install dotslash
  # or: prebuilt packages — https://dotslash-cli.com/docs/installation/
  /usr/bin/env dotslash --help   # sanity check
  ```

- **protoc** — proto codegen resolves [`bin/protoc`](bin/protoc) via DotSlash
  (Linux, macOS Apple Silicon, macOS Intel, Windows x64). On Windows, if the
  DotSlash wrapper cannot be executed, install `protoc` on `PATH` or set
  `$PROTOC` / `%PROTOC%` to `protoc.exe`. Windows ARM64 has no protobuf 29.3
  zip in this wrapper; use a PATH `protoc`.
- Supported build hosts: Linux (x86_64 / aarch64), **Apple Silicon macOS**
  (`aarch64-apple-darwin`, M1–M5), Intel macOS (`x86_64-apple-darwin`), and
  Windows x64. Kernel sandboxing (Landlock / Seatbelt) is Unix-only; Windows
  runs with the same process-level helpers grok-build uses there. Native Apple
  Silicon builds are preferred; a Rosetta x86_64 process is detected and the
  updater installs arm64 instead.

```sh
cargo run -p xai-grok-pager-bin              # build + launch the TUI
cargo build -p xai-grok-pager-bin --release  # release binary: target/release/grok-local
cargo check -p xai-grok-pager-bin            # fast validation
```

The binary artifact is named `grok-local` (`grok-local.exe` on Windows) so it
does not collide with an official `grok` install. Local LM Studio inference is
used at startup; xAI browser login is skipped. See the
[authentication guide](crates/codegen/xai-grok-pager/docs/user-guide/02-authentication.md).

## Documentation

Full online documentation is available at
[docs.x.ai/build/overview](https://docs.x.ai/build/overview).

The user guide ships with the pager crate:
[`crates/codegen/xai-grok-pager/docs/user-guide/`](crates/codegen/xai-grok-pager/docs/user-guide/)
— getting started, keyboard shortcuts, slash commands, configuration, theming,
MCP servers, skills, plugins, hooks, headless mode, sandboxing, and more.

## SystemOne native routing (v0.5.2)

Two independent selectors, settable per session or per CLI invocation:

| Selector | Choices | Default |
|---|---|---|
| Model | `auto` or a pinned model | pinned |
| Thinking | `off` / `low` / `medium` / `high` / `xhigh` / `ultra` / `auto` | `auto` |

Every task — `grok-local --single` headless prompts and interactive/ACP
per-turn prompts alike — routes through the local **SystemOne** router
(`http://127.0.0.1:8765`, the `knowledgator/gliclass-edge-v3.0` classifier)
**natively**: the router client is the embedded `xai-grok-systemone` Rust
crate, compiled into this binary — no external adapter to wire, no config
entries to add, no shim to launch by hand. If the router is not running, the
binary starts the installed shim itself (detached; the model is already in
the HuggingFace cache, so cold start is seconds).

The returned tier maps to a reasoning effort and loop cap
(`edge`/`economy` → low / 4 turns, `balanced` → medium / 6, `heavy` →
high / 10). Headless: applied only when `--reasoning-effort` / `--max-turns`
were not passed explicitly. Interactive: the routed effort auto-tunes each
turn unless you explicitly set one (then the router stands down); the routed
turn cap applies unless `--max-turns` was passed. Route-driven MCP server
suggestions are logged; actual pruning of the per-session MCP list (headless
only) is opt-in and conservative (live router, ≥ 0.85 confidence, cheap tier
only).

Fail-open always: a router outage or error leaves the session exactly as if
routing did not exist. One greppable `systemone: …` line on stderr proves
what routing did (source, tier, effort, max_turns, confidence).

Under `thinking=auto` the router picks the effort per task and may reach
`xhigh`/`ultra` for heavy work; a pinned level (`/thinking high`,
`--thinking low`) makes the router stand down on effort and that level is
applied instead. Under `model=auto` (`/model auto`, `--model auto`) the
router's model pick is advisory only — it is logged and shown in the status
line, never acted on. This binary never unloads or switches your loaded
model.

Kill-switches (env wins over `~/.grok-local/config.toml` `[systemone]`):

| Env | Effect |
|---|---|
| `GROK_LOCAL_SYSTEMONE=0` | Disable all routing behavior |
| `GROK_LOCAL_SYSTEMONE_NO_AUTOSTART=1` | Probe only; never start the shim |
| `GROK_LOCAL_SYSTEMONE_PRUNE=1` | Opt in to conservative MCP pruning |

The selectors are also settable via env: `GROK_LOCAL_SYSTEMONE_THINKING`
(`off|low|medium|high|xhigh|ultra|auto`) and
`GROK_LOCAL_SYSTEMONE_MODEL_SELECTION` (`auto|pinned`) — env wins over the
config file, same precedence as the kill-switches above.

### Decision engine (SystemOne)

The decision engine behind the routing is **SystemOne**: a **Jev-style**
typed-decision layer over local GLiClass checkpoints — the "tiny decides,
big works" split. A small classifier answers in ~100 ms with *scored*
decisions: per-tier probabilities, top-1/top-2 margins, and an uncertainty
flag. The scores are calibrated (temperature/Platt fitting), then drive
tier, effort, expected-utility model ranking, tool/MCP relevance ranking,
and `rank-plans` plan ranking. Advisory-only, fail-open: a down or slow
shim changes nothing about the session. The engine lives in the Python
shim (`python -m systemone.shim`, `http://127.0.0.1:8765`) that this binary
auto-starts.

**Jeff-1 second decision head.** The shim's second head is
**[Jeff-1](https://huggingface.co/GestaltLabs/Jeff-1)** (GestaltLabs,
Apache 2.0) — an open-weight model (LoRA on
[Qwen3-4B-Instruct-2507](https://huggingface.co/Qwen/Qwen3-4B-Instruct-2507))
trained for Jev-compatible typed decisions (`choice`, `score`, `noul`);
published model-card metrics: accuracy 0.8183, macro-F1 0.7789, Brier
0.2839, ECE 0.0807. `rank-plans` blends Jeff-1's P(plan succeeds | task)
50/50 with the GLiClass score, and *uncertain* routes record an advisory
`jeff1_second_opinion` — the routed tier is never changed by it. Certain
routes never touch Jeff-1 (trivial tasks stay on the fast path). It runs
as a sidecar process (`:8079`), on by default (`SYSTEMONE_JEFF1=0` runs
GLiClass-only), and fails open to GLiClass-only when down or slow. It
never loads, unloads, switches, or competes with your loaded model — it
lives on the Mac mini, never on the inference host.

`ranked_tools` from the shim drives MCP/server suggestions (shim-ranked
provenance); `ranked_models` is advisory-only.

## Repository layout

| Path | Contents |
|------|----------|
| `crates/codegen/xai-grok-pager-bin` | Composition-root package; builds the `grok-local` binary |
| `crates/codegen/xai-grok-pager` | The TUI: scrollback, prompt, modals, rendering |
| `crates/codegen/xai-grok-shell` | Agent runtime + leader/stdio/headless entry points |
| `crates/codegen/xai-grok-tools` | Tool implementations (terminal, file edit, search, ...) |
| `crates/codegen/xai-grok-workspace` | Host filesystem, VCS, execution, checkpoints |
| `crates/codegen/...` | The rest of the CLI crate closure (config, MCP, markdown, sandbox, ...) |
| `crates/common/`, `crates/build/`, `prod/mc/` | Small shared leaf crates pulled in by the closure |
| `third_party/` | Vendored upstream source (Mermaid diagram stack) — see below |

> [!IMPORTANT]
> The root `Cargo.toml` (workspace members, dependency versions, lints,
> profiles) is **generated** — treat it as read-only. Prefer editing per-crate
> `Cargo.toml` files.

## Development

```sh
cargo check -p <crate>        # always target specific crates; full-workspace builds are slow
cargo test -p xai-grok-config # per-crate tests
cargo clippy -p <crate>       # lint config: clippy.toml at the repo root
cargo fmt --all               # rustfmt.toml at the repo root
```

> [!NOTE]
> The `xai-grok-shell` turn-loop/retry tests run under a paused Tokio clock
> and need headroom above the fork's 2s local connect timeout (see
> `GROK_CONNECT_TIMEOUT_SECS` in `xai-grok-sampler`):
>
> ```sh
> GROK_CONNECT_TIMEOUT_SECS=10 cargo test -p xai-grok-shell --features test-support
> ```

## Contributing

> [!NOTE]
> External contributions are not accepted. See [`CONTRIBUTING.md`](CONTRIBUTING.md).

## License

First-party code in this repository is licensed under the **Apache License,
Version 2.0** — see [`LICENSE`](LICENSE).

Third-party and vendored code remains under its original licenses. See:

- [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES) — crates.io / git dependencies,
  bundled UI themes, and **in-tree source ports** (including openai/codex and
  sst/opencode tool implementations)
- [`crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md`](crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md)
  — crate-local notice for the codex and opencode ports (license texts +
  Apache §4(b) change notice)
- [`third_party/NOTICE`](third_party/NOTICE) — vendored Mermaid-stack index
