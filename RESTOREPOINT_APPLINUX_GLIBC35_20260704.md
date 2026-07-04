# Restore Point: Linux AppImage glibc-2.35 launch fix + macOS zip dedupe

Date: 2026-07-04
User confirmation: "it launches in 3 seconds this is acceptable"
Launched artifact: /tmp/ci-appimg-22/decode-rust-gui-linux_2.0.0-2-g2ff83a3_x86_64.AppImage

## Problem
1. AppImage built on `ubuntu-latest` (24.04, glibc 2.39) failed to launch on glibc 2.35 hosts with:
   `Failed to load Python shared library 'libpython3.12.so.1.0': version GLIBC_2.38 not found`
2. `build.yml` macOS job produced a redundant zip-of-DMG alongside the DMG (duplicate deliverables).

## Root cause (verified with hard data)
- Host glibc: 2.35 (Ubuntu 22.04 base / Linux Mint)
- Old CI AppImage bundled libpython3.12.so.1.0 requiring GLIBC_2.38 (built on ubuntu-24.04).
- My local build (on the glibc 2.35 host) launched fine -> isolated the failure to the CI build-host glibc.
- PyQt6 x86_64 wheel is `manylinux_2_34` (portable to glibc 2.34+); aarch64 wheel is only `manylinux_2_39` (no portable arm64 wheel exists).

## Fix applied (commits on master)
- `a995144` fix(ci): build AppImage on ubuntu-22.04 for glibc portability; drop redundant macOS DMG zips
- `2ff83a3` fix(ci): keep arm64 AppImage on ubuntu-24.04-arm (PyQt6 aarch64 wheel needs glibc 2.39)

## Files changed
- .github/workflows/build.yml
  - Linux x86_64 AppImage matrix: runner ubuntu-latest -> ubuntu-22.04
  - macOS packaging: removed zip-of-DMG step; upload DMG only
  - Release body/files: 5 GUI artifacts (1 Windows exe, 2 Linux ZIPs, 2 macOS DMGs)
- .github/workflows/build_linux_decode.yml
  - Linux x86_64 matrix: runner ubuntu-latest -> ubuntu-22.04
  - arm64 stays ubuntu-24.04-arm (upstream PyQt6 aarch64 wheel requires glibc 2.39)

## Verification (hard data)
- Host glibc: ldd (Ubuntu GLIBC 2.35-0ubuntu3.13) 2.35
- Old CI AppImage (ubuntu-24.04 build) launch: EXIT=255, `GLIBC_2.38 not found`
- New CI AppImage (ubuntu-22.04 build) launch: EXIT=124 (alive 12s, no GLIBC error)
- User real-world confirmation: launches in 3 seconds, acceptable.
- CI run build.yml 28710397861: success (all jobs)
- CI run build_linux_decode.yml 28710398433: success
- macOS artifact sizes after zip removal: arm64 81484196 bytes, x86_64 99431157 bytes (duplicates gone)

## Restore artifact
- Working AppImage preserved in: RESTOREPOINT_APPLINUX_GLIBC35_20260704.zip
  (contains the verified-launching x86_64 AppImage built on ubuntu-22.04)
- Build provenance: CI run 28710397861, job Linux decode-rust-gui AppImage (x86_64), SHA 2ff83a3
- Version string: decode-rust-gui-linux_2.0.0-2-g2ff83a3_x86_64

## Known limitation
- arm64 AppImage still requires glibc 2.39 (upstream PyQt6 aarch64 wheel constraint; no portable wheel exists). Not fixable in workflow config alone.

## Restore instructions
To return to this known-good state:
1. `git reset --hard 2ff83a3` on master (or reapply the two commits above)
2. Unzip RESTOREPOINT_APPLINUX_GLIBC35_20260704.zip to recover the working x86_64 AppImage
3. Verify launch on a glibc 2.35 host: `./decode-rust-gui-linux_*_x86_64.AppImage` (should launch in ~3s)
