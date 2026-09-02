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
defaults to SearxNG at `http://127.0.0.1:8888` (`SEARXNG_URL`; a trailing
`/search` is stripped). Do not point this at Open WebUI on `:8080`.

A small `SOURCE_REV` file at the root records the grok-build monorepo SHA
in this tree. `grok-local --version` prints **both** the grok-local fork
version and the grok-build version that overlay was applied on.

```sh
grok-local --version
# grok-local 0.4.1 (<git sha>)
# grok-build 1.0.16 (<SOURCE_REV>)
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

## Repository layout

| Path | Contents |
|------|----------|
| `crates/codegen/xai-grok-pager-bin` | Composition-root package; builds the `xai-grok-pager` binary |
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
