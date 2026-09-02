#!/usr/bin/env python3
"""Overlay-preserving sync from github.com/xai-org/grok-build.

Three-way merge:
  base   = grok-build at the last synced GitHub commit
  other  = grok-build origin/main
  ours   = this grok-local tree

Fork-only paths (.github, integrations, overlay-only sources) are never
replaced by upstream. Official `grok` binaries are never downloaded.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

SKIP_DIRS = {".git", "target"}
PRESERVE_TOP = {".github", "integrations", "scripts", "overlays"}
FORK_ONLY_FILES = {
    "crates/codegen/xai-grok-shell/src/agent/models/lm_studio.rs",
    "crates/codegen/xai-grok-tools/src/implementations/web_search/searxng.rs",
}

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MIRROR = Path.home() / ".grok-local" / "upstream" / "grok-build"
UPSTREAM_URL = "https://github.com/xai-org/grok-build.git"
UPSTREAM_SHA_FILE = REPO_ROOT / "overlays" / "UPSTREAM_SHA"
OVERLAY_LIST = REPO_ROOT / "overlays" / "files.txt"


def run(cmd: list[str], cwd: Path | None = None, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, cwd=cwd, check=check, text=True, capture_output=True)


def git_out(args: list[str], cwd: Path) -> str:
    return run(["git", *args], cwd=cwd).stdout.strip()


def rel_files(root: Path) -> set[str]:
    out: set[str] = set()
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        p = Path(dirpath)
        rel_dir = p.relative_to(root)
        if rel_dir.parts and rel_dir.parts[0] in SKIP_DIRS:
            continue
        for name in filenames:
            rel = Path(name) if rel_dir == Path(".") else rel_dir / name
            out.add(rel.as_posix())
    return out


def is_preserved(rel: str) -> bool:
    top = rel.split("/", 1)[0]
    return top in PRESERVE_TOP or rel in FORK_ONLY_FILES


def ensure_mirror(mirror: Path) -> None:
    if (mirror / ".git").exists():
        run(["git", "fetch", "origin"], cwd=mirror)
        return
    mirror.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["git", "clone", UPSTREAM_URL, str(mirror)],
        check=True,
    )


def checkout_worktree(mirror: Path, sha: str, dest: Path) -> None:
    if dest.exists():
        shutil.rmtree(dest)
    dest.parent.mkdir(parents=True, exist_ok=True)
    run(["git", "worktree", "prune"], cwd=mirror, check=False)
    result = subprocess.run(
        ["git", "worktree", "add", "--detach", str(dest), sha],
        cwd=mirror,
        text=True,
        capture_output=True,
    )
    if result.returncode != 0:
        dest.mkdir(parents=True, exist_ok=True)
        run(["git", "archive", sha], cwd=mirror).stdout  # type: ignore[attr-defined]
        archive = subprocess.run(
            ["git", "archive", sha],
            cwd=mirror,
            check=True,
            capture_output=True,
        )
        subprocess.run(["tar", "-x", "-C", str(dest)], input=archive.stdout, check=True)


def file_bytes(path: Path) -> bytes | None:
    try:
        return path.read_bytes()
    except FileNotFoundError:
        return None


def merge_file(ours: Path, base: Path, other: Path) -> int:
    ours.parent.mkdir(parents=True, exist_ok=True)
    if not ours.exists() and other.exists():
        shutil.copy2(other, ours)
        return 0
    cmd = ["git", "merge-file", "-L", "grok-local", "-L", "base", "-L", "grok-build", str(ours), str(base), str(other)]
    proc = subprocess.run(cmd)
    return proc.returncode


def classify(local: Path, base: Path) -> tuple[list[str], list[str], list[str]]:
    local_files = {r for r in rel_files(local) if not is_preserved(r)}
    base_files = rel_files(base)
    overlay: list[str] = []
    identical: list[str] = []
    for rel in sorted(local_files & base_files):
        if file_bytes(local / rel) != file_bytes(base / rel):
            overlay.append(rel)
        else:
            identical.append(rel)
    fork_only = sorted(local_files - base_files)
    return overlay, identical, fork_only


def write_overlay_list(overlay: list[str], fork_only: list[str]) -> None:
    OVERLAY_LIST.parent.mkdir(parents=True, exist_ok=True)
    lines = ["# Paths grok-local owns. sync-upstream.py three-way-merges these.\n"]
    for rel in overlay:
        lines.append(rel + "\n")
    lines.append("# fork-only (never replaced by grok-build)\n")
    for rel in fork_only:
        lines.append(rel + "\n")
    OVERLAY_LIST.write_text("".join(lines))


def sync(local: Path, mirror: Path, check_only: bool) -> int:
    ensure_mirror(mirror)
    run(["git", "fetch", "origin"], cwd=mirror)
    latest = git_out(["rev-parse", "origin/main"], mirror)
    latest_short = latest[:12]
    recorded = UPSTREAM_SHA_FILE.read_text().strip() if UPSTREAM_SHA_FILE.exists() else ""
    # Prefer recorded SHA; otherwise the grok-build tree that matches SOURCE_REV.
    if recorded:
        base_sha = recorded
    else:
        # Walk origin/main history for the commit whose SOURCE_REV matches ours.
        local_rev = (local / "SOURCE_REV").read_text().strip()
        base_sha = None
        log = git_out(["log", "--format=%H", "origin/main"], mirror)
        for sha in log.splitlines():
            show = subprocess.run(
                ["git", "show", f"{sha}:SOURCE_REV"],
                cwd=mirror,
                text=True,
                capture_output=True,
            )
            if show.returncode == 0 and show.stdout.strip() == local_rev:
                base_sha = sha
                break
        if base_sha is None:
            # Last resort: current checkout of a sibling grok-build clone.
            sibling = Path.home() / "grok-build"
            if (sibling / ".git").exists():
                base_sha = git_out(["rev-parse", "HEAD"], sibling)
            else:
                print("error: cannot find grok-build commit matching SOURCE_REV", local_rev, file=sys.stderr)
                return 2

    if latest == base_sha:
        print(f"already synced to grok-build {latest_short}")
        if check_only:
            return 0
        return 0

    print(f"grok-build {base_sha[:12]} -> {latest_short}")
    if check_only:
        new_rev = subprocess.run(
            ["git", "show", f"{latest}:SOURCE_REV"],
            cwd=mirror,
            text=True,
            capture_output=True,
            check=True,
        ).stdout.strip()
        new_ver = subprocess.run(
            ["git", "show", f"{latest}:crates/codegen/xai-grok-version/Cargo.toml"],
            cwd=mirror,
            text=True,
            capture_output=True,
            check=True,
        ).stdout
        ver = next((ln.split("=", 1)[1].strip().strip('"') for ln in new_ver.splitlines() if ln.startswith("version")), "?")
        print(f"update available: grok-build {ver} SOURCE_REV {new_rev}")
        return 0

    work = Path("/tmp") / f"grok-local-sync-{os.getpid()}"
    base_dir = work / "base"
    other_dir = work / "other"
    try:
        checkout_worktree(mirror, base_sha, base_dir)
        checkout_worktree(mirror, latest, other_dir)

        overlay, identical, _detected_fork_only = classify(local, base_dir)
        fork_only = sorted(FORK_ONLY_FILES)
        write_overlay_list(overlay, fork_only)
        print(f"overlay={len(overlay)} identical={len(identical)} fork-only={len(fork_only)}")

        other_files = rel_files(other_dir)
        base_files = rel_files(base_dir)
        conflicts = []

        # Replace files that grok-local did not touch.
        for rel in identical:
            if rel not in other_files:
                path = local / rel
                if path.exists():
                    path.unlink()
                continue
            dest = local / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(other_dir / rel, dest)

        # New upstream files (not overlay, not preserved).
        for rel in sorted(other_files - base_files):
            if is_preserved(rel):
                continue
            dest = local / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(other_dir / rel, dest)

        # Overlay three-way merge.
        for rel in overlay:
            ours = local / rel
            base = base_dir / rel
            other = other_dir / rel
            if not other.exists():
                print(f"keep overlay (removed upstream): {rel}")
                continue
            if not base.exists():
                print(f"keep overlay (no base): {rel}")
                continue
            rc = merge_file(ours, base, other)
            if rc != 0:
                conflicts.append(rel)
                print(f"CONFLICT {rel}")

        # Drop upstream-deleted files that were identical (already handled).
        # Restore fork-only files: they were never in the replace set.

        UPSTREAM_SHA_FILE.write_text(latest + "\n")
        src_rev = (other_dir / "SOURCE_REV").read_text()
        (local / "SOURCE_REV").write_text(src_rev)
        print(f"SOURCE_REV {src_rev.strip()}")
        if conflicts:
            print(f"{len(conflicts)} overlay conflicts — search for <<<<<<< grok-local")
            return 1
        print("overlay merge clean")
        return 0
    finally:
        shutil.rmtree(work, ignore_errors=True)
        run(["git", "worktree", "prune"], cwd=mirror, check=False)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="report whether grok-build is ahead")
    parser.add_argument("--local", type=Path, default=REPO_ROOT)
    parser.add_argument("--mirror", type=Path, default=DEFAULT_MIRROR)
    parser.add_argument(
        "--base-dir",
        type=Path,
        help="existing grok-build checkout to use as merge base (skips git worktree)",
    )
    parser.add_argument(
        "--other-dir",
        type=Path,
        help="existing grok-build checkout of origin/main (skips git worktree)",
    )
    args = parser.parse_args()
    local = args.local.resolve()
    if args.base_dir and args.other_dir:
        return sync_dirs(local, args.base_dir.resolve(), args.other_dir.resolve(), args.check)
    return sync(local, args.mirror.resolve(), args.check)


def sync_dirs(local: Path, base_dir: Path, other_dir: Path, check_only: bool) -> int:
    other_sha = git_out(["rev-parse", "HEAD"], other_dir) if (other_dir / ".git").exists() else "unknown"
    base_sha = git_out(["rev-parse", "HEAD"], base_dir) if (base_dir / ".git").exists() else "unknown"
    print(f"grok-build {base_sha[:12]} -> {other_sha[:12]}")
    if check_only:
        print("update available" if base_sha != other_sha else "already synced")
        return 0
    overlay, identical, _detected_fork_only = classify(local, base_dir)
    fork_only = sorted(FORK_ONLY_FILES)
    write_overlay_list(overlay, fork_only)
    print(f"overlay={len(overlay)} identical={len(identical)} fork-only={len(fork_only)}")
    other_files = rel_files(other_dir)
    base_files = rel_files(base_dir)
    conflicts = []
    for rel in identical:
        if rel not in other_files:
            path = local / rel
            if path.exists():
                path.unlink()
            continue
        dest = local / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(other_dir / rel, dest)
    for rel in sorted(other_files - base_files):
        if is_preserved(rel):
            continue
        dest = local / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(other_dir / rel, dest)
    for rel in overlay:
        ours = local / rel
        base = base_dir / rel
        other = other_dir / rel
        if not other.exists():
            print(f"keep overlay (removed upstream): {rel}")
            continue
        if not base.exists():
            print(f"keep overlay (no base): {rel}")
            continue
        rc = merge_file(ours, base, other)
        if rc != 0:
            conflicts.append(rel)
            print(f"CONFLICT {rel}")
    UPSTREAM_SHA_FILE.parent.mkdir(parents=True, exist_ok=True)
    if other_sha != "unknown":
        UPSTREAM_SHA_FILE.write_text(other_sha + "\n")
    shutil.copy2(other_dir / "SOURCE_REV", local / "SOURCE_REV")
    print("SOURCE_REV", (local / "SOURCE_REV").read_text().strip())
    if conflicts:
        print(f"{len(conflicts)} overlay conflicts — search for <<<<<<< grok-local")
        return 1
    print("overlay merge clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
