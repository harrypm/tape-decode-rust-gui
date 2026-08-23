#!/usr/bin/env python3
# generate_release_notes.py
#
# Build a granular, commit-based changelog for a GitHub release from git
# history, grouped by component, with each change citing its commit hash.
# Mirrors the tbc-tools release-notes style (oyvindln/vhs-decode family).
#
# GitHub's releases/generate-notes API only lists merged pull requests and
# "New Contributors", which omits every direct commit pushed to the default
# branch. This script walks `git log PREVIOUS_TAG..RELEASE_TAG` and groups
# the actual commits so the release body reflects what really shipped.
#
# The GitHub API output is still fetched by the caller and passed in via
# --api-notes-file so the "New Contributors" section can be preserved (that
# part is genuinely useful and not reproducible from git alone).
#
# Usage:
#   scripts/release/generate_release_notes.py \
#       --repo harrypm/tape-decode-rust-gui \
#       --tag v0.7.0 \
#       --previous-tag v0.6.0 \
#       --api-notes-file /tmp/api-notes.md \
#       --output /tmp/release-notes.md
#
# Exit codes: 0 on success, non-zero on error.

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from typing import Optional


# Component buckets, in display order. The first matcher that accepts a
# subject wins, so order matters (put specific components before the generic
# CI/packaging and Other buckets).
COMPONENT_MATCHERS: list[tuple[str, "ComponentMatcher"]] = []


def register(name: str):
    def decorator(func: "ComponentMatcher") -> "ComponentMatcher":
        COMPONENT_MATCHERS.append((name, func))
        return func

    return decorator


ComponentMatcher = callable  # type alias


def _matches_any(subject: str, *needles: str) -> bool:
    lowered = subject.lower()
    return any(needle.lower() in lowered for needle in needles)


@register("metadata")
def _metadata(subject: str) -> bool:
    # .tbc.json sidecar fields and the profile -> Medium/Format/System mapping.
    return _matches_any(
        subject,
        "metadata",
        "sidecar",
        "tbc.json",
        "gitbranch",
        "gitcommit",
        "gitrelease",
        "video parameters",
        "medium",
    )


@register("decode-rust-gui")
def _decode_rust_gui(subject: str) -> bool:
    # The PyQt6 launcher, its PyInstaller bundling, and per-platform packaging
    # of the launcher (AppImage / DMG / Windows EXE). Checked before
    # tape-decode and CI/packaging so launcher/bundling commits land here even
    # when they mention "windows"/"linux"/"build".
    return _matches_any(
        subject,
        "decode-rust-gui",
        "decode launcher",
        "launcher",
        "pyqt",
        "pyinstaller",
        "appimage",
        "dmg",
        "self-contained",
        "bundle",
        "gui",
    )


@register("tape-decode")
def _tape_decode(subject: str) -> bool:
    # The Rust core decoder + CLI. Avoid bare "decode" (it matches
    # "decode-rust-gui", which is already bucketed above) and bare "rf".
    return _matches_any(
        subject,
        "tape-decode",
        "tape decode",
        "decoder",
        "demod",
        "deemp",
        "deemphasis",
        "chroma",
        "hsync",
        "vsync",
        "wow",
        "dropout",
        "vits",
        "vbi",
        "flac reader",
        "flac writer",
        "list-profiles",
        "write-profile",
        "fields match",
        "multithread",
        "microarch",
    )


@register("CI / packaging")
def _ci_packaging(subject: str) -> bool:
    return _matches_any(
        subject,
        "ci(",
        "ci:",
        "release",
        "packaging",
        "workflow",
        "build.yml",
        "windows",
        "macos",
        "linux",
        "selftest",
        "nix",
        "cleanup",
        "chore(release)",
    )


def classify_component(subject: str) -> str:
    for name, matcher in COMPONENT_MATCHERS:
        try:
            if matcher(subject):
                return name
        except Exception:
            continue
    return "Other"


# Commits whose subjects are release-process noise and should not appear in the
# user-facing changelog.
NOISE_PATTERNS = [
    re.compile(r"^chore\(release\):\s*prepare", re.IGNORECASE),
    re.compile(r"^Merge pull request", re.IGNORECASE),
    re.compile(r"^Merge branch", re.IGNORECASE),
    re.compile(r"^Merge tag", re.IGNORECASE),
    re.compile(r"^chore:\s*bump version", re.IGNORECASE),
]


def is_noise(subject: str) -> bool:
    return any(pattern.match(subject.strip()) for pattern in NOISE_PATTERNS)


@dataclass
class Commit:
    sha: str
    subject: str
    component: str


@dataclass
class Changelog:
    release_tag: str
    previous_tag: Optional[str]
    repo: str
    commits_by_component: dict[str, list[Commit]] = field(default_factory=dict)
    truncated: bool = False


def run_git(args: list[str], cwd: Optional[str] = None) -> str:
    try:
        result = subprocess.run(
            ["git", *args],
            check=True,
            capture_output=True,
            text=True,
            cwd=cwd,
        )
    except subprocess.CalledProcessError as exc:
        sys.stderr.write(
            f"git {' '.join(args)} failed with exit {exc.returncode}:\n"
            f"{exc.stderr.strip()}\n"
        )
        raise
    return result.stdout


def collect_commits(
    release_tag: str,
    previous_tag: Optional[str],
    max_commits: int = 400,
    cwd: Optional[str] = None,
) -> tuple[list[Commit], bool]:
    # %x09 is a TAB; we split on the first tab so subjects may contain colons.
    fmt = "%h%x09%s"
    if previous_tag:
        rev_range = f"{previous_tag}..{release_tag}"
    else:
        rev_range = release_tag

    raw = run_git(
        ["log", "--no-merges", f"--max-count={max_commits}", f"--pretty=format:{fmt}", rev_range],
        cwd=cwd,
    )

    commits: list[Commit] = []
    for line in raw.splitlines():
        if not line.strip():
            continue
        sha, _, subject = line.partition("\t")
        sha = sha.strip()
        subject = subject.strip()
        if not sha or not subject:
            continue
        if is_noise(subject):
            continue
        commits.append(Commit(sha=sha, subject=subject, component=classify_component(subject)))

    total = int(
        run_git(["rev-list", "--no-merges", "--count", rev_range], cwd=cwd).strip() or "0"
    )
    truncated = total > max_commits

    return commits, truncated


# Component display order. "Other" sorts last.
def component_sort_key(name: str) -> tuple[int, str]:
    preferred = [
        "metadata",
        "decode-rust-gui",
        "tape-decode",
        "CI / packaging",
        "Other",
    ]
    try:
        return (preferred.index(name), name)
    except ValueError:
        return (len(preferred), name)


def render_changelog(changelog: Changelog) -> str:
    lines: list[str] = []
    lines.append("## What's Changed")
    lines.append("")

    ordered_components = sorted(
        changelog.commits_by_component.keys(), key=component_sort_key
    )

    if not ordered_components:
        lines.append("_No user-facing commits detected in this range._")
        lines.append("")

    for component in ordered_components:
        commits = changelog.commits_by_component[component]
        if not commits:
            continue
        lines.append(f"### {component}")
        for commit in commits:
            # Escape backticks in the subject so inline code stays valid.
            safe_subject = commit.subject.replace("`", "'")
            lines.append(f"- {safe_subject} (`{commit.sha}`)")
        lines.append("")

    if changelog.truncated:
        lines.append(
            f"_Note: the changelog was truncated to the most recent "
            f"{len(sum(changelog.commits_by_component.values(), []))} non-merge commits; "
            f"see the full compare link below for the complete history._"
        )
        lines.append("")

    return "\n".join(lines)


def extract_new_contributors(api_notes: str) -> Optional[str]:
    """Pull the '## New Contributors' section out of GitHub's auto notes.

    The GitHub generate-notes output also includes a "**Full Changelog**" line,
    which this script regenerates itself; strip it from the extracted block so
    the final notes do not contain a duplicate compare link.
    """
    if not api_notes:
        return None
    match = re.search(
        r"(## New Contributors.*?)(?=\n## |\Z)",
        api_notes,
        flags=re.DOTALL,
    )
    if not match:
        return None
    block = match.group(1).strip()
    # Drop any trailing "**Full Changelog**" line the API appended after the
    # New Contributors section (it appears un-headed before EOF).
    block = re.sub(r"\*\*Full Changelog\*\*:.*$", "", block, flags=re.IGNORECASE).strip()
    return block if block else None


def render_full_changelog_link(repo: str, previous_tag: Optional[str], release_tag: str) -> str:
    if previous_tag:
        return (
            f"**Full Changelog**: https://github.com/{repo}/compare/"
            f"{previous_tag}...{release_tag}"
        )
    return f"**Full Changelog**: https://github.com/{repo}/commits/{release_tag}"


def build_notes(
    repo: str,
    release_tag: str,
    previous_tag: Optional[str],
    api_notes: str,
    cwd: Optional[str] = None,
) -> str:
    commits, truncated = collect_commits(release_tag, previous_tag, cwd=cwd)
    by_component: dict[str, list[Commit]] = {}
    for commit in commits:
        by_component.setdefault(commit.component, []).append(commit)

    changelog = Changelog(
        release_tag=release_tag,
        previous_tag=previous_tag,
        repo=repo,
        commits_by_component=by_component,
        truncated=truncated,
    )

    parts: list[str] = [render_changelog(changelog).rstrip()]

    new_contributors = extract_new_contributors(api_notes)
    if new_contributors:
        parts.append("")
        parts.append(new_contributors)

    parts.append("")
    parts.append(render_full_changelog_link(repo, previous_tag, release_tag))
    parts.append("")  # trailing newline

    return "\n".join(parts)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Generate granular, commit-based release notes from git history."
    )
    parser.add_argument("--repo", required=True, help="OWNER/NAME of the GitHub repository.")
    parser.add_argument("--tag", required=True, help="Release tag being published (e.g. v0.7.0).")
    parser.add_argument(
        "--previous-tag",
        default=None,
        help="Previous release tag to diff from (e.g. v0.6.0). Omit for first release.",
    )
    parser.add_argument(
        "--api-notes-file",
        default=None,
        help="Path to GitHub releases/generate-notes API output, used to extract New Contributors.",
    )
    parser.add_argument("--output", required=True, help="Path to write the generated notes markdown.")
    parser.add_argument(
        "--cwd",
        default=None,
        help="Repository working directory. Defaults to the current directory.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    api_notes = ""
    if args.api_notes_file and os.path.exists(args.api_notes_file):
        with open(args.api_notes_file, "r", encoding="utf-8") as handle:
            api_notes = handle.read()

    try:
        notes = build_notes(
            repo=args.repo,
            release_tag=args.tag,
            previous_tag=args.previous_tag,
            api_notes=api_notes,
            cwd=args.cwd,
        )
    except subprocess.CalledProcessError:
        return 1

    with open(args.output, "w", encoding="utf-8") as handle:
        handle.write(notes)

    sys.stdout.write(f"Wrote release notes to {args.output}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
