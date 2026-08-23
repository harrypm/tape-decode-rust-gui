# PROMPT — tbc-tools-style release notes (20260823)

Log of inputs, commands, and findings for adding proper "what's new"
release notes to builds, tbc-tools style.

## User input
- "add proper 'whats new' tbc-tools style release notes to builds."

## Key findings (hard data)
- tbc-tools is harrypm/tbc-tools (your own repo). Its v3.2.3 release notes
  show the style: header, **Highlights**, **What's Changed** grouped by area
  with commit subjects + short shas, **New Contributors**, **Full Changelog**
  compare link.
- tbc-tools generates notes via `scripts/release/generate_release_notes.py`:
  walks `git log PREV..NEW --no-merges`, groups by component (first matcher
  wins), drops noise subjects, pulls **New Contributors** from GitHub's
  `releases/generate-notes` API output, appends its own Full Changelog link.
  Its `release.yml` orchestrates: resolve tag → call build workflows →
  `upload-release-assets` job generates notes + `gh release --notes-file`.
- This repo was fragmented: `build.yml` had a unified release job (hand-written
  body, fires only on workflow_dispatch), and each per-OS workflow had its own
  release job (Win/Mac fire on tag push with auto-notes, Linux workflow_dispatch
  only). On a v* tag push, Win + Mac each created/edited the same release with
  GitHub auto-notes, Linux wasn't published, build.yml's curated body never
  appeared.
- build.yml's build jobs lacked the `--selftest` gate, and its linux-decode
  job was missing `libegl1 libgl1 libglvnd0` (would have regressed the
  GL-lib self-containment fix when the release moved onto build.yml).
- User decision: consolidate to ONE release (build.yml produces it; per-OS
  workflows become build-only).

## Changes implemented
- `scripts/release/generate_release_notes.py` (NEW): ported tbc-tools' script
  with tape-decode-rust-gui component buckets (first matcher wins):
  **metadata**, **decode-rust-gui**, **tape-decode**, **CI / packaging**,
  **Other**. Same noise filters (`chore(release): prepare`, `Merge ...`,
  `chore: bump version`), `collect_commits`, `extract_new_contributors`,
  `render_full_changelog_link`, CLI args.
- `.github/workflows/build.yml`:
  - windows-decode + macos-decode: added the `--selftest` step
    (`QT_QPA_PLATFORM=offscreen`, strict grep `SELFTEST OK`).
  - linux-decode: added `libegl1 libgl1 libglvnd0` to the apt install; added
    the `--selftest` step; replaced the `|| true` AppImage list-profiles
    check with a strict `--selftest`.
  - Windows pip: dropped unused `pyinstaller-versionfile`.
  - Release job: now fires on tag push too
    (`startsWith(github.ref, 'refs/tags/') || workflow_dispatch && create_release`).
    Replaced the static body + `generate_release_notes` with: resolve previous
    tag (`git describe --tags --abbrev=0 --match "v*" "${tag}^"`), call
    `gh api /repos/{repo}/releases/generate-notes` for New Contributors, run
    `python3 scripts/release/generate_release_notes.py`, `gh release create/
    edit --notes-file`, `gh release upload` all assets (clobber),
    `gh release edit --latest` (continue-on-error).
- `.github/workflows/build_{windows,macos,linux}_decode.yml`: removed the
  `release:` job and `push: tags: - "v*"` (build.yml owns the release); removed
  the now-dead `create_release`/`release_tag` inputs. Kept `workflow_dispatch`
  for standalone build/selftest testing.

## Commands run
- `python -m py_compile scripts/release/generate_release_notes.py` -> OK
- `python scripts/release/generate_release_notes.py --repo harrypm/tape-decode-rust-gui --tag HEAD --previous-tag v3.0.0 --output %TEMP%\release-notes-preview.md` -> exit 0; output was exactly tbc-tools style:
  `## What's Changed` grouped (metadata / decode-rust-gui / CI / packaging),
  each line `- <subject> (`<sha>`)`, trailing
  `**Full Changelog**: https://github.com/harrypm/tape-decode-rust-gui/compare/v3.0.0...HEAD`.

## Verification
- [x] generate_release_notes.py compiles and runs; produces grouped What's
      Changed + Full Changelog link against the real repo history.
- [x] YAML edits applied to all 4 workflows; working tree shows only the 4
      modified workflow files + new scripts/release/ dir.
- [ ] End-to-end release run: NOT executed (would publish a real GitHub
      release / tag). Needs a `v*` tag push or workflow_dispatch with
      create_release=true to exercise the new release job on CI.

## Notes / out of scope
- Not converting build.yml to a `workflow_call` orchestrator (tbc-tools' exact
  shape); build.yml keeps its own build jobs (lower-risk, still one release).
- Not adding a hand-curated **Highlights** section; the tbc-tools script
  doesn't produce one (v3.2.3's Highlights were added manually). Generated
  body = What's Changed (grouped) + New Contributors + Full Changelog.
- Not committed (user has not requested commit for this task).
- Per-OS workflows still build all 6 matrix entries on workflow_dispatch, so
  the selftest gate can be tested independently of a release.
