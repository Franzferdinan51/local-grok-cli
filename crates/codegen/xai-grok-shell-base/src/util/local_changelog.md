# Grok Local 0.4.5

- Fixed the local fork startup update prompt so it no longer offers an xAI upstream version or routes `Ctrl+U` through the upstream installer.

- Auto-update now keeps xAI upstream checks separate from local fork releases. `grok-local` automatically updates only from the local fork, while upstream source changes remain available through `grok-local update --upstream`.

## Features

- Startup, `--version`, and `/session-info` show **Grok Local** and **Grok Build** versions together.
- `grok-local update` installs binaries from [Franzferdinan51/local-grok-cli releases](https://github.com/Franzferdinan51/local-grok-cli/releases).
- `grok-local update --upstream` overlay-merges [xai-org/grok-build](https://github.com/xai-org/grok-build) without replacing LM Studio, SearxNG, `~/.grok-local`, or the `grok-local` name.
- Welcome changelog lists Grok Local notes first, then the current Grok Build notes.
