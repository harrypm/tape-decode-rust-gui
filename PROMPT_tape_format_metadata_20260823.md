# PROMPT — tape format metadata flags (20260823)

Log of inputs, commands, and findings for the request to add Medium /
Format / System / git-branch / git-commit flags to the decode JSON sidecar,
derived from the selected decode profile name.

## User input
- "Add tape format metdata based off decoding profile name triggered to JSON
  for Medium (TAPE) / Format (VHS, BETAMAX etc) / System (NTSC / PAL / MESECAM)
  flags also Git Branch and Git Commit version flags/names should be written for
  clear chain of metadata."
- "Also add SECAM to System and 405, 819"
- "its the profile list info" (the source of the flags = the profile name)
- "This needs to be clean and simple the context to set the flags just needs to
  be mapped to metadata fields"
- "Also add to metadata gitRelease for the current release tag version to the
  metadata"

## Key findings (hard data from source)
- `videoParameters` in `crates/tape-decode-cli/src/metadata.rs` already had
  `system`, `tapeFormat`, `gitBranch`, `gitCommit` (all String) but they were
  stubbed: `tapeFormat` was hardcoded `"TAPE"` (the medium, not the format);
  `system` only resolved to "NTSC"/"PAL"/"PAL-M" via `DecoderMetadata.system`
  (`crates/tape-decode/src/decode/mod.rs:1618-1622`) — no SECAM/MESECAM/405/
  819/MPAL/NLINHA; `gitBranch`/`gitCommit` were hardcoded `"UNKNOWN"`.
- Profile names in `profiles.json` follow `<SYSTEM>_<FORMAT>[_<SPEED>]`:
  systems = 405, 819, MESECAM, MPAL, NTSC, PAL, SECAM, NLINHA; speed suffixes
  = EP, LP, VP. e.g. `SECAM_VHS_LP` -> system "SECAM", format "VHS";
  `405_BETAMAX` -> system "405", format "BETAMAX";
  `NTSC_BETAMAX_HIFI` -> system "NTSC", format "BETAMAX_HIFI" (HIFI is part of
  the format, not a speed).
- `tape-decode-cli` had no `build.rs` (no git injection). `tape-decode/build.rs`
  only checks nightly.
- `scripts/ci/git-version.sh` is the project's version resolver: honor
  `DECODE_LIGHT_VERSION(_OVERRIDE)`, else `git describe --tags --dirty --match
  'v*' --match 'decode-light-*' --match 'decode-rust-gui-*'`, else
  `dev-<sha>[-dirty]`; strips `decode-light-`/`decode-rust-gui-` prefixes.
  `gitRelease` mirrors this so it matches release artifact versions.
- `compare` (`cli.rs:877-895`) already skips system/gitBranch/gitCommit/
  tapeFormat in its diff, so changing these values does not break compare.

## Changes implemented
- `crates/tape-decode-cli/build.rs` (NEW):
  - runs `git rev-parse --abbrev-ref HEAD` / `git rev-parse HEAD`, emits
    `cargo:rustc-env=GIT_BRANCH=...` / `GIT_COMMIT=...` (fallback "UNKNOWN").
  - `git_release()` ports `scripts/ci/git-version.sh`: honors
    `DECODE_LIGHT_VERSION(_OVERRIDE)`, else `git describe --tags --dirty
    --match 'v*' --match 'decode-light-*' --match 'decode-rust-gui-*'`, else
    `dev-<short-sha>[-dirty]`; strips `decode-light-`/`decode-rust-gui-`
    prefixes. Emits `cargo:rustc-env=GIT_RELEASE=...`.
  - Emits `rerun-if-changed` for the workspace `.git/HEAD`.
- `crates/tape-decode-cli/src/metadata.rs`:
  - Added `MetadataContext { medium, format, system, git_branch, git_commit,
    git_release }`.
  - Added `parse_profile_flags(name) -> (&str, &str)`: splits the profile name
    on `_`; first segment = system; drops a trailing EP/LP/VP speed segment;
    remainder = format. Medium is always "TAPE".
  - Added `MetadataContext::from_profile_name(Option<&str>)` (custom
    `--profile-file` -> "UNKNOWN"/"custom"); reads git_branch/git_commit/
    git_release via `option_env!`.
  - Added `#[serde(default)] medium: String` and `#[serde(default)]
    git_release: String` (renamed `gitRelease`) to `VideoParameters` (so old
    sidecars still deserialize).
  - Changed `metadata_to_tbc` to take `&MetadataContext` and populate
    `system`, `tape_format` (now the real format), `medium`, `git_branch`,
    `git_commit`, `git_release` from it (no longer hardcoded / no longer from
    `DecoderMetadata.system`).
  - 4 unit tests: parse_profile_flags across all systems + speed suffixes;
    metadata_to_tbc writes the context flags; custom profile file; and a
    serialization test asserting the camelCase JSON keys
    (`"system":"405"`, `"tapeFormat":"BETAMAX"`, `"medium":"TAPE"`,
    `"gitBranch":"..."`, `"gitCommit":"..."`, `"gitRelease":"..."`) + that
    git_branch/git_commit/git_release injection is real (non-UNKNOWN, non-empty).
- `crates/tape-decode-cli/src/writer.rs`: `DecodeWriter` stores the
  `MetadataContext`; `new()` accepts it; `append_tail` passes it to
  `metadata_to_tbc` (both call sites updated).
- `crates/tape-decode-cli/src/cli.rs`: `run_decode` builds
  `MetadataContext::from_profile_name(cli.profile.as_deref())` and passes it to
  `DecodeWriter::new`.

## Commands run
- `cargo build --release --bin tape-decode` -> BUILD_EXIT=0 (1m12s); build.rs
  injected git branch/commit/release.
- `cargo test --release -p tape-decode-cli` -> 4 passed; 0 failed.
- `git describe --tags --dirty --match 'v*' ...` -> `v3.0.0-5-gb5f7b0d-dirty`
  (existing v3.0.0 tag, 5 commits ahead, dirty from uncommitted feature
  files). So GIT_RELEASE currently = `v3.0.0-5-gb5f7b0d-dirty`; on a clean
  release-tag commit it would be just the tag (e.g. `v3.0.1`).

## Verification
- [x] `cargo build --release --bin tape-decode` succeeds.
- [x] 4 unit tests pass, including serialization test that confirms the actual
      JSON bytes contain `"system":"405"`, `"tapeFormat":"BETAMAX"`,
      `"medium":"TAPE"`, `"gitBranch":"..."`, `"gitCommit":"..."` and that the
      build.rs injected non-"UNKNOWN" git values.
- [ ] end-to-end `.tbc.json` from a real RF decode: NOT run (no sample RF
      capture available). The writer calls the same `metadata_to_tbc` +
      `serde_json::to_vec` path the serialization test exercises, so the JSON
      content is verified; a real decode would exercise the identical path once
      `decoder.metadata()` returns Some (>=1 field decoded). Needs a sample RF
      file to produce a sidecar.

## Notes / out of scope
- `DecoderMetadata.system` (library public API in `tape-decode`) is left in
  place; it is no longer the source of the JSON `system` (the profile-name
  context is). Noted in a comment in `metadata_to_tbc`.
- `osInfo` left empty (not requested).
- Python launcher unchanged; it just invokes the binary.
- Not committed (user has not requested commit for this feature).
