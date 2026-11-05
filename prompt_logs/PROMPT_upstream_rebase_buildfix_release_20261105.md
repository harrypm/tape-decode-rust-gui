# Prompt Log — Upstream Rebase + Build Workflow Fix + New Release
**Date:** 2026-11-05
**Repo:** /home/harry/tape-decode-rust (fork: harrypm/tape-decode-rust-gui, upstream: namazso/tape-decode-rs)
**Task:** Rebase master with upstream again, fix the builds workflow, make a new release.

## Inputs
- User request: "rebase with upstream again.. and fix the builds workflow and make a new release"
- PWD: /home/harry/tape-decode-rust, branch master, up-to-date with fork/master at 60aec61.

## Commands run and outputs (running log)

### 1. Initial repo state
```
git status && git remote -v && git branch -a
```
- On branch master, up-to-date with fork/master.
- Untracked: PROMPT_commit_push_dispatch_v3.0.0.md, RELEASE_REBUILD_V2.0.0_20260711.md, RESTOREPOINT zips/mds, decode-rust-gui zips.
- Remotes: fork = harrypm/tape-decode-rust-gui, origin = namazso/tape-decode-rs.
- Backup branches exist: backup/pre-upstream-rebase-20261101, backup/pre-upstream-rebase-20261102.

### 2. Recent history check
```
git --no-pager log --oneline -15
git --no-pager log --oneline -5 origin/master
```
- Local master: 60aec61 (docs: record push of the 20261102 rebase round) at HEAD, custom commits on top of older upstream base.
- Upstream at fetch time: 3278b6f (decode: drop the per-line plumbing of usectoinpx).

### 3. Fetch upstream and count drift
```
git fetch origin
git rev-list --count master..origin/master
```
- origin/master advanced 3278b6f -> 88fcd87.
- **88 new upstream commits** since the 20261102 rebase.
- Notable upstream changes: "test: remove the unit tests" (88fcd87), field/sync rework commits, CLI option changes (required values, negative values), profile validation, FLAC seek-to-end, raw TBC export changes.

### 4. Local commit count on top of merge-base
```
git rev-list --count origin/master..master
```
- **65 local custom commits** (SECAM direct/CVBS, GUI, docs, restorepoints).

### 5. Repo structure, logs folder, tags
```
ls /home/harry/tape-decode-rust; ls prompt_logs; git tag --list; gh release list
```
- Logs folder is `prompt_logs/` (this file lives there).
- Tags: v1.0.0, v2.0.0, v2.0.0b, v3.0.0, v4.0.0, v4.0.1, v4.1.0 (+ a bare `v` tag).
- Existing releases (fork): v1.0.0 through **v4.1.0 (latest, ~26 days ago)**. Next release candidate: **v4.2.0**.

## Work plan
1. Create this prompt log (done).
2. Inspect .github/workflows (build.yml, build_linux_decode.yml, build_macos_decode.yml, build_windows_decode.yml) and check recent fork workflow runs for failures.
3. Create backup branch backup/pre-upstream-rebase-20261105 (+ restore point artifacts per convention).
4. Rebase master onto origin/master (88 commits), resolve conflicts (expect: tests removed upstream vs our test dummy fix 95f5f0b; decode/cli rework vs SECAM direct-composite).
5. Fix the builds workflow(s) so all platform builds pass.
6. Verify build locally (cargo build / check) and via workflow runs.
7. Tag v4.2.0 and create the GitHub release with build artifacts.
8. Move prompt log into prompt_logs, commit, push to fork.

## Outputs

### Rebase (completed)
- 65 fork commits replayed onto 88 new upstream commits (3278b6f..88fcd87).
- Conflicts resolved in 5 commits: 812119a + c1289b6 (cli.rs metadata wiring onto upstream's
  restructured option-check-first layout), f3bd1a0 (spec.rs imports + is_secam_direct;
  chroma.rs infallible process_chroma_internal + SECAM-direct early exit onto upstream's
  downscale_raw_vec; field.rs profile-driven linecounts superseding our 405/819 match,
  dropped obsolete ire0_backporch, kept SECAM burst-lock skip), db0d3d9 (spec.rs
  rf_write_chroma = is_color_under && !secam_direct per upstream a37e2b0; cli.rs CVBS
  exemption from upstream's new monochrome --chroma-out refusal).
- Post-rebase fix commit 1889bcb: dropped stale `butter_sos` decode-module import
  (upstream moved the designer into spec.rs).
- Full resolution log: REBASE_UPSTREAM_20261105.md.

### Verification (hard data)
- cargo check/build --workspace --release: clean. cargo test: 5 passed, 0 failed.
- list-profiles: all 70 profiles present (SECAM_QUADRUPLEX/TYPEC, MESECAM_*, QUAD/TYPEC, 405/819).
- SECAM_QUADRUPLEX real-data decode (etienne colorbars 50f): 399 fields in 54.2s;
  --chroma-out warn-and-ignore verified; single combined .tbc (283 MB); sidecar
  system=SECAM tapeFormat=QUADRUPLEX rfSourceSampleRate=40e6 gitCommit=677b80f.
- PAL_SVHS real-data decode (EBU bars): 33 fields in 13.2s — matches documented
  pre-rebase result exactly; sidecar intact.
- Profile round-trip: direct/cvbs flags identical to the 20261102 validated matrix
  (QUADRUPLEX/TYPEC direct+cvbs True; SECAM_VHS/MESECAM_VHS False; NTSC_TYPEC cvbs True).
- MESECAM_VHS on tape275 sample: 0 fields — A/B against the pre-rebase backup binary
  (worktree build) gives the identical 0-field result: sample/profile mismatch, NOT
  a rebase regression (FLAC header rate is 17.9 kHz; sample needs different setup).
- decode_selftest.py offscreen: SELFTEST OK.

### Builds workflow fix (commit b09b2c1)
- Root cause: PyQt6-Qt6 6.12.0 wheel framework layout breaks PyInstaller onefile
  extraction on macOS (QtQuickControls2Macos.framework file/symlink duplicate,
  selftest exit 255, both macOS jobs of run 41934681262; Windows/Linux green).
- Last green (v4.1.0, run 37978873053): PyQt6 6.11.0 + PyQt6-Qt6 6.11.2 + PyInstaller 6.22.3.
- Fix: requirements-launcher.txt pins PyQt6>=6.6,<6.12 and PyQt6-Qt6>=6.6,<6.12
  (covers all platform jobs; all install via this file).

### Tags repointed (lightweight, subject-matched)
- v, v2.0.0b -> 36a1d6d; v1.0.0 -> 0c5223f; v2.0.0 -> 8fd41a2; v3.0.0 -> 4c4f8d9;
  v4.0.0 -> 79bcf32; v4.0.1 -> 2a1b94a; v4.1.0 -> 2194be2.

### Push (completed)
- fork/master force-with-lease: 60aec61 -> 36b7e88 (fix commits 1889bcb, b09b2c1,
  restorepoint 36b7e88 on top of the replayed history).
- All 8 tags force-pushed; backup/pre-upstream-rebase-20261105 pushed.
- Stray root prompt log PROMPT_commit_push_dispatch_v3.0.0.md moved into prompt_logs/.

### CI validation run (no release) — ALL GREEN
- Run 42017089081 on master (create_release=false):
  - Linux x86_64 10m2s, Linux arm64 3m47s, Windows x86_64 11m58s, Windows arm64 4m34s,
    macOS x86_64 22m28s, macOS arm64 5m20s — 6/6 ✓
- macOS x86_64 log hard data: `Successfully installed PyQt6-6.11.0 PyQt6-Qt6-6.11.2
  ... pyinstaller-6.22.3 pyinstaller-hooks-contrib-2026.9` + `SELFTEST OK` —
  pin resolves to the last-good wheels (hooks-contrib 2026.9 works with 6.11,
  confirming the 6.12.0 wheel layout was the culprit).

### Release v4.2.0 — PUBLISHED
- Release run 42020310485 (create_release=true, release_tag=v4.2.0): 6/6 build jobs ✓
  + Create GitHub release ✓ (59s).
- https://github.com/harrypm/tape-decode-rust-gui/releases/tag/v4.2.0 — latest,
  non-draft, notes generated for v4.1.0..v4.2.0 by the workflow's notes generator.
- Assets (13): decode-rust-gui linux/macOS/windows × x86_64/arm64 (6) + tbc-tools
  v3.3.0 bundle (7).
- Tag v4.2.0 created by the workflow at the released master SHA.

### Session end state
- master = 88 upstream + 65 replayed fork + 3 round commits (1889bcb, b09b2c1, 36b7e88)
  + this log commit; fork/master in sync; backup branch + bundle preserved.
- Logs copied to FUTO Notes AI Notebook.
