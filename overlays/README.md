# grok-local overlay

This fork tracks [xai-org/grok-build](https://github.com/xai-org/grok-build) without installing the official `grok` binary.

- `SOURCE_REV` — grok-build monorepo SHA currently in the tree
- `overlays/UPSTREAM_SHA` — GitHub commit of grok-build last merged
- `overlays/files.txt` — paths grok-local owns (three-way merged on update)
- `scripts/sync-upstream.py` — overlay-preserving sync

```sh
# grok-local GitHub Releases (binaries)
grok-local update --check
grok-local update

# grok-build source overlay (keeps this fork's patches)
grok-local update --upstream --check
grok-local update --upstream
# or: python3 scripts/sync-upstream.py
```

Official `curl https://x.ai/cli/install.sh` / `grok update` would replace this binary. Do not use them here.
