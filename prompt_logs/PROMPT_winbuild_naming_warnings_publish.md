# Prompt log: fix Win build naming + arm64, clear GH Actions warnings, investigate publish

Date: 2026-08-11

## User input / task
- "I also want to fix the Win build naming, and fix it for the ARM64 win builds and also clear the GH actions warnings and investigate why the last run did not publish"
- Prior command: `python3 /home/harry/tape-decode-rust/decode_launcher.py` (launched OK; only Qt ICC/style warnings, unrelated to prior --subdeemp change).

## Commands run (by assistant)
- `gh auth status` -> authenticated as harrypm.
- `git remote -v` + `gh repo view` -> fork is `harrypm/tape-decode-rust` (origin = namazso/tape-decode-rs).
- `gh run list --repo harrypm/tape-decode-rust --limit 20` -> recent runs. Latest build.yml = 30690939074 (2026-08-01, success). Latest build_windows_decode.yml = 29139051721 (v2.0.0, 2026-07-11, success).
- `gh run view <rid> --json jobs` for 30690939074, 29139051721, 29139051722, 29139051724 -> per-job conclusions. All build.yml 30690939074 jobs success incl. "Create GitHub release".
- `gh release list` + `gh release view v3.0.0` -> **v3.0.0 is a Draft, untagged** (URL `releases/tag/untagged-2f123cc4ae7bc2d6f302`), created 2026-08-01. Assets: linux/macos x86_64+arm64 + `decode-rust-gui-windows_3.0.0_x86_64.exe` (bare exe, NO arm64 windows).
- `gh api .../check-runs/<jid>/annotations` across latest build.yml run -> warning: "Node.js 20 is deprecated. actions/upload-artifact@v4 targets Node.js 20 but is being forced to Node.js 24."
- `grep -rEho "uses: ...@..." .github/workflows/*.yml` -> action version inventory.
- `python3 -m yaml.safe_load` on all 4 workflow files -> all parse OK (after edits).
- `grep` for deprecated refs -> confirmed none remain after edits.

## Investigation findings (verified from hard data)
1. **Why last run did not publish**: `build.yml` release job had `draft: true` (line 515). The 2026-08-01 workflow_dispatch run (30690939074, create_release=true) created an **untagged DRAFT** v3.0.0 that never auto-publishes. The published v2.0.0 release was published manually on 2026-07-11.
2. **Windows naming inconsistency**: `build.yml` produced `decode-rust-gui-windows_<ver>_x86_64.exe` (v stripped, bare exe, x86_64 only). `build_windows_decode.yml` produced `decode-rust-gui-windows_v<ver>_<arch>.zip` (v KEPT, zipped, x86_64+arm64). Linux/macOS canonical = `decode-rust-gui-<os>_<ver>_<arch>.<ext>` v stripped + zipped. Windows was the odd one in BOTH workflows.
3. **GH Actions warning**: `actions/upload-artifact@v4` (Node 20, deprecated). Also `actions/download-artifact@v4` and `softprops/action-gh-release@v2` in build.yml. build_windows_decode.yml already on clean `upload@v7`/`download@v8`/`gh-release@v3` (no annotations).

## User decisions (via ask_user_question)
- Publish fix: "Set build.yml release to draft:false so workflow_dispatch+create_release publishes automatically".
- Windows fix: "Upgrade build.yml's Windows job to an x86_64+arm64 matrix producing decode-rust-gui-windows_<ver>_<arch>.zip (v stripped), matching Linux/macOS".

## Edits made

### `.github/workflows/build.yml`
- **windows-decode job**: rewritten from single x86_64 to matrix (x86_64 windows-latest + arm64 windows-11-arm). Single binary for arm64, v1-v4 levels for x86_64. Arch-aware smoke test (v1 forced on x86_64, plain on arm64). `Test Rust core` scoped to x86_64 (preserves original behavior; arm64 covered by smoke test). `setup-python` uses `architecture: ${{ matrix.python_arch }}`. `rust-toolchain` uses `targets: ${{ matrix.target }}`.
- **Packaging**: replaced bare `.exe` copy with pwsh `Compress-Archive` -> `decode-rust-gui-windows_<ver>_<arch>.zip` (flat, 1-entry validated). v stripped (existing `VERSION="${VERSION#v}"`).
- **Upload**: `actions/upload-artifact@v4` -> `@v7`, artifact name `decode-rust-gui-windows-${{ matrix.arch }}`, `if-no-files-found: error`.
- **release job**: `download-artifact@v4` -> `@v8`; `softprops/action-gh-release@v2` -> `@v3`; `draft: true` -> `draft: false`; Windows release globs changed from `*.exe` (x86_64 only) to `*.zip` for x86_64 + arm64; release body updated to "2 Windows ZIPs" / "6 GUI artifacts".
- **linux-decode + macos-decode upload steps**: `upload-artifact@v4` -> `@v7`.

### `.github/workflows/build_linux_decode.yml`
- `upload-artifact@v4` -> `@v7` (Upload Linux ZIP step).
- `download-artifact@v4` -> `@v8` (release job).

### `.github/workflows/build_macos_decode.yml`
- `upload-artifact@v4` -> `@v7` (Upload macOS DMG step).
- `download-artifact@v4` -> `@v8` (release job).

### `.github/workflows/build_windows_decode.yml`
- Version step: added `VERSION="${VERSION#v}"` so artifacts are `decode-rust-gui-windows_<ver>_<arch>.zip` (matches build.yml + Linux/macOS). Avoids two differently-named Windows assets when both workflows publish on workflow_dispatch+create_release.

## Validation
- `python3` YAML parse: all 4 workflow files OK.
- `grep` for `upload-artifact@v4|download-artifact@v4|action-gh-release@v2|@v3` across `.github/workflows`: **no matches** (all cleared).
- Windows naming: both workflows now emit `decode-rust-gui-windows_<ver>_<arch>.zip` (v stripped).
- NOT validated by running the workflows yet (requires push/dispatch).

## Pending / NOT done (needs user decision — live release action)
- The existing **v3.0.0 draft** is still unpublished and incomplete (no Windows arm64 asset, bare exe not zip). Options for the user:
  1. Delete the stale v3.0.0 draft, then dispatch build.yml with create_release=true (now publishes a complete v3.0.0 with Windows x86_64+arm64 zips, draft:false).
  2. Publish v3.0.0 as-is via `gh release edit v3.0.0 --draft=false` (ships incomplete — not recommended).
  3. Leave the draft; push a real `v3.0.0` git tag (triggers per-OS workflows that publish non-draft).
- I did NOT run any `gh release`/`gh run workflow` command — those are user-facing release actions and need explicit confirmation.

## Side note
- The earlier `--subdeemp` launcher change (decode_launcher.py) launched OK; only Qt warnings (`fromIccProfile: failed size sanity 2`, `invalid style override 'Adwaita-Dark'`) which are environment/Qt-related, not from the checkbox change.
