# PROMPT — bundle tbc-tools binaries into the release workflow (20260828)

Log of inputs, commands, and findings for mirroring vhs-decode's
tbc-tools release-asset bundling in tape-decode-rust's `build.yml`.

## User input
- "add tbx-tools binarys to the release workflow likw vhs-decode hass"
- Clarification asked: "tbx-tools" repo does not exist on GitHub; vhs-decode
  pulls from `harrypm/tbc-tools`. User confirmed:
  `harrypm/tbc-tools (same as vhs-decode uses)`.

## Key findings (hard data)
- `harrypm/tbx-tools` does NOT exist:
  `GraphQL: Could not resolve to a Repository with the name 'harrypm/tbx-tools'.`
  "tbx-tools" was a typo for tbc-tools.
- `harrypm/tbc-tools` exists; description: "Software defined post decoder
  tools for the decode projects tbc format and metadata pipeline from 4fsc
  to YUV video."
- vhs-decode `release.yml` pattern (the template to mirror):
  - top-level `env: TBC_TOOLS_REPO: "harrypm/tbc-tools"`
  - `upload-release-assets` job: `gh api repos/${TBC_TOOLS_REPO}/releases/latest`
    resolves latest tag, then `gh release download <tag> --repo <repo>
    --dir ./artifacts/tbc-tools`, validates the dir is non-empty
    (`compgen -G ./artifacts/tbc-tools/*`), copies assets into the staging
    dir, and `gh release upload <tag> ./artifacts/release/* --clobber`.
  - Assets are uploaded verbatim under their original tbc-tools versioned
    names (separate project, own versioning).
- tbc-tools latest release (`v3.2.7`) ships 7 assets:
  - `tbc-tools_v3.2.7_linux_arm64.tar.xz`
  - `tbc-tools_v3.2.7_linux_x86_64.tar.xz`
  - `tbc-tools_v3.2.7_macos_arm64.dmg`
  - `tbc-tools_v3.2.7_macos_universal.dmg`
  - `tbc-tools_v3.2.7_macos_x86_64.dmg`
  - `tbc-tools_v3.2.7_windows_arm64.zip`
  - `tbc-tools_v3.2.7_windows_x86_64.zip`
  => extensions are `.tar.xz`, `.dmg`, `.zip`.
- tape-decode-rust `build.yml` already had a `release` job that downloads
  build artifacts with `actions/download-artifact@v8` (path: `artifacts`)
  and uploads via `find artifacts -type f \( -name '*.zip' -o -name '*.dmg' \)`
  + `gh release upload ... --clobber`. That find would MISS the tbc-tools
  Linux `.tar.xz` assets, so the find pattern had to be broadened.
- The `release` job only runs on `startsWith(github.ref, 'refs/tags/')` or
  `workflow_dispatch && create_release == true`, so the tbc-tools download
  only happens on an actual publish — correct placement.

## Changes implemented
- `.github/workflows/build.yml`:
  1. Top-level `env`: added `TBC_TOOLS_REPO: "harrypm/tbc-tools"`
     (mirrors vhs-decode; inherited by the release job).
  2. New step in the `release` job, placed after "Download build artifacts"
     and before "Generate release notes":
     `Download latest tbc-tools release assets` — `mkdir -p artifacts/tbc-tools`,
     `gh api repos/${TBC_TOOLS_REPO}/releases/latest --jq '.tag_name'` (fail
     if empty), `gh release download <tag> --repo <repo> --dir artifacts/tbc-tools`,
     validate non-empty with `compgen -G artifacts/tbc-tools/*`, `ls -1` the
     result. Mirrors vhs-decode's download + staging validation.
  3. "Upload release assets" step: broadened the find to
     `\( -name '*.zip' -o -name '*.dmg' -o -name '*.tar.xz' \)` so the
     tbc-tools Linux `.tar.xz` assets are included alongside the existing
     decode `.zip` / `.dmg` assets. Upload still uses
     `gh release upload "$RELEASE_TAG" $ASSETS --clobber`.

## Commands run
- `gh repo view harrypm/tbc-tools --json nameWithOwner,description` -> exists.
- `gh repo view harrypm/tbx-tools ...` -> GraphQL error (repo not found).
- `gh release view --repo harrypm/tbc-tools --json tagName,name,assets` ->
  tag `v3.2.7`, 7 assets listed above.
- `gh api repos/harrypm/tbc-tools/contents/.github/workflows --jq '.[].name'`
  -> build_linux_tools.yml, build_macos_tools.yml, build_windows_tools.yml,
  publish_cuda_plugin.yml, release.yml, tests.yml.
- `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/build.yml'))"`
  -> `YAML OK`; top-level env keys `['FORCE_JAVASCRIPT_ACTIONS_TO_NODE24',
  'TBC_TOOLS_REPO']`; release job steps include
  `Download latest tbc-tools release assets` in the right position.
- `command -v actionlint` -> NOT installed (no stricter lint run).

## Verification
- [x] `build.yml` parses as valid YAML after the edits.
- [x] `TBC_TOOLS_REPO` present in top-level env; new download step present
      in the `release` job between "Download build artifacts" and
      "Generate release notes"; upload find includes `*.tar.xz`.
- [ ] End-to-end release run: NOT executed (would create/upload to a real
      GitHub release). Needs a `v*` tag push or workflow_dispatch with
      create_release=true to exercise the new step on CI. The download step
      relies on `harrypm/tbc-tools` being public (verified) and the runner's
      default `GITHUB_TOKEN` reading it (same as vhs-decode).

## Notes / out of scope
- tbc-tools assets keep their own versioned filenames (e.g.
  `tbc-tools_v3.2.7_linux_x86_64.tar.xz`) in the tape-decode-rust release —
  intentional, matches vhs-decode; tbc-tools is a separate project with its
  own release cadence and the workflow always pulls "latest".
- Did NOT restructure tape-decode-rust's release job into vhs-decode's
  flat `artifacts/release/` staging-dir shape; kept the existing find-based
  upload (lower-risk, proven) and just added the tbc-tools download step +
  one extra find extension. Net effect is the same: all tbc-tools assets
  uploaded verbatim.
- If a future tbc-tools release ships a new asset extension (e.g. `.tar.gz`
  or `.AppImage`), the find pattern in "Upload release assets" would need
  that extension added; current extensions (zip/dmg/tar.xz) are all covered.
- Not committed (user has not requested a commit for this task).
