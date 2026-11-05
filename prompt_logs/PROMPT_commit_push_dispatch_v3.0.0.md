# Prompt log: commit, push, clean stale v3.0.0 draft, dispatch rebuild

Date: 2026-08-11 (follow-up to PROMPT_winbuild_naming_warnings_publish.md)

## User input / task
- "Commit the changes and push the branch to the repository."
- "commit and push and clean stale draft" (option 1 from prior prompt: delete stale v3.0.0 draft, then dispatch build.yml with create_release=true to publish a complete v3.0.0).

## Pre-state
- Branch: master, tracking fork/master (fork = harrypm/tape-decode-rust, renamed to harrypm/tape-decode-rust-gui on GitHub).
- Modified (task changes): .github/workflows/build.yml, build_linux_decode.yml, build_macos_decode.yml, build_windows_decode.yml, decode_launcher.py.
- Untracked (NOT committed — user's pre-existing WIP/build artifacts): RELEASE_REBUILD_V2.0.0_20260711.md, RESTOREPOINT_APPLINUX_GLIBC35_20260704.zip, RESTOREPOINT_V2.0.0_REBUILD_20260711.md/.zip, decode-rust-gui-linux_dev-6147f05_x86_64.zip, decode-rust-gui-linux_localtest-symlink_x86_64.zip.
- Stale release: v3.0.0 draft=true, tagName=v3.0.0 but URL=untagged-2f123cc4ae7bc2d6f302 (git tag v3.0.0 does NOT exist on fork or locally).

## Commands run + results
1. `git status --short` / `branch --show-current` -> master, 5 modified files + untracked WIP.
2. `git add decode_launcher.py PROMPT_subdeemp_checkbox.md && git commit` -> commit b4e59b9 "launcher: add --subdeemp checkbox, enabled by default" (2 files, +81).
3. `git add .github/workflows/*.yml PROMPT_winbuild_naming_warnings_publish.md && git commit` -> commit 8c6e726 "ci: fix Windows arm64 build naming, clear Actions warnings, auto-publish" (5 files, +164/-44).
4. `git push fork master` -> 63c5265..8c6e726 master -> master. Remote noted rename to tape-decode-rust-gui (redirect, push OK).
5. `gh release view v3.0.0` -> draft=true, tag=v3.0.0, untagged URL.
6. `git ls-remote fork refs/tags/v3.0.0` -> empty (no git tag). `git tag -l v3.0.0` -> empty.
7. `gh release delete v3.0.0 --cleanup-tag --yes` -> HTTP 422 on tag cleanup (tag didn't exist), but release was deleted first. Verified: `gh release view v3.0.0` -> "release not found".
8. `gh workflow run build.yml --ref master -f create_release=true -f release_tag=v3.0.0` -> "Created workflow_dispatch event for build.yml at master", run id 31547193190, URL https://github.com/harrypm/tape-decode-rust-gui/actions/runs/31547193190.
9. `gh run view 31547193190` -> sha=8c6e726, branch=master, event=workflow_dispatch, jobs: macOS DMG (arm64), Windows (arm64), Linux AppImage (x86_64), macOS DMG (x86_64), Linux AppImage (arm64), Windows (x86_64). Release job not yet listed (needs: build jobs).

## Commits pushed
- b4e59b9 launcher: add --subdeemp checkbox, enabled by default
- 8c6e726 ci: fix Windows arm64 build naming, clear Actions warnings, auto-publish

## What happens next (not yet verified)
- Run 31547193190 builds 6 OS jobs then the release job publishes v3.0.0 (draft:false) with assets:
  - decode-rust-gui-windows_3.0.0_x86_64.zip (NEW)
  - decode-rust-gui-windows_3.0.0_arm64.zip (NEW)
  - decode-rust-gui-linux_3.0.0_x86_64.zip
  - decode-rust-gui-linux_3.0.0_arm64.zip
  - decode-rust-gui-macos_3.0.0_x86_64.dmg
  - decode-rust-gui-macos_3.0.0_arm64.dmg
- Monitor: `gh run watch 31547193190 --repo harrypm/tape-decode-rust` or the URL above.
- This log file was NOT committed (host-system reference only); the two prompt logs covering the actual code/CI changes were committed with their respective commits.
