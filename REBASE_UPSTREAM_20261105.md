# Upstream Rebase 20261105 (tape-decode-rs namazso) - Prompt Log

**Started:** 2026-11-05 10:37 UTC
**User request:** rebase with upstream again, fix the builds workflow, make a new release
**Rule applied:** prompt readme.md with all output, input, and commands run (see
`prompt_logs/PROMPT_upstream_rebase_buildfix_release_20261105.md` for the full session log).

## Pre-rebase state (hard data)
- HEAD: `60aec61` (`master`, `fork/master`) — in sync with fork, tracked tree clean
- Previous rebases: 20261101 (base c0c80ce), 20261102 (base 3278b6f)
- Upstream fetch: `3278b6f..88fcd87` → **88 new upstream commits**, local **65 commits** ahead
- Merge base: `3278b6f`
- Backup: branch `backup/pre-upstream-rebase-20261105` + `RESTOREPOINT_PRE_REBASE_20261105.bundle`

## Notable new upstream commits (88)
88fcd87 test: remove the unit tests
b8f4086 field: judge hsyncs on the field's grid, the vertical interval's pulses on the line before
96bf66b decode: take the --ire0-adjust windows from the profile's sync timing
6ca6108 scale_field: start the level adjustment from the field's median wow factor
6752e46 sync: match the vertical interval's pulses to the raw pulses by position
a37e2b0 decode: keep the burst-locked hsync refinement for the raw TBC export
9a57f81 spec: design the custom shelving filters at the sample rate
69db770 reader: seek a FLAC input to its end instead of failing there
a8e3636 cli: refuse a chroma output on a monochrome profile
26fcd7d request: reject unknown profile keys
852824e spec: check the profile's geometry and nonlinear domains at construction
8df4236 field: take the field's line count from the profile's field lines
ec1ae10 sync: take the 405 and 819 systems down the line-0 fallback's 625 branches
(+ 75 more, full list in git log)

Upstream now carries its own ME-SECAM carrier servo (`secam_lo_trim`,
`SECAM_LO_TRIM_LIMIT_HZ`), 405/819 line systems, profile-driven field line counts,
and its own `DecoderParams`-level SECAM direct definitions that align with ours.

## Rebase stops and resolutions (5 commits conflicted)
1. `812119a` (metadata Medium/Format/System) — `cli.rs`: upstream moved
   spec/reader/start_offset creation ahead of the output files; kept upstream's
   structure, added only `MetadataContext::from_profile_name` + 4-arg
   `DecodeWriter::new`.
2. `c1289b6` (rfSourceSampleRate) — `cli.rs`: same pattern; kept
   `rf_source_sample_rate_hz` + 2-arg `from_profile_name` on upstream's structure.
3. `f3bd1a0` (SECAM direct-composite R1):
   - `spec.rs`: import union (+`SECAM_LO_TRIM_LIMIT_HZ`); kept `is_secam_direct()`
     beside upstream's `tbc_sample_rate` doc fix.
   - `chroma.rs`: upstream made `process_chroma_internal` infallible and inlined the
     old `tracked_chroma` as `downscale_raw_vec(.., true, shift, None)`; took
     upstream's extraction, re-added the `is_secam_direct()` early exit
     (`spec.burst_area` field instead of the old helper, no `Ok()`/`?`).
   - `field.rs`: upstream's profile-driven linecount/geometry superseded our
     405/819 hardcoded match (dropped); dropped obsolete `ire0_backporch` field
     initializer (upstream moved the windows to spec-level `Ire0AdjustWindows`);
     kept our validated SECAM burst-lock skip (`rf_write_chroma && color_system != Secam`)
     on upstream's unified structure.
4. `db0d3d9` (CVBS single .tbc):
   - `spec.rs`: import union (+`SECAM_BLOCK_CENTRE`); `rf_write_chroma =
     is_color_under && !secam_direct` — upstream's a37e2b0 removed their own
     `!rf_export_raw_tbc` term (burst-locked hsync refinement now kept under the raw
     export), so only the fork's CVBS exclusion remains.
   - `cli.rs` (auto-merge follow-up fix): upstream's new monochrome `--chroma-out`
     refusal (a8e3636) ran before our CVBS warn-and-ignore and would have
     hard-failed CVBS profiles; excluded `cvbs_single_output` from the bail.
5. Post-rebase compile fix: `butter_sos` moved into spec.rs by upstream — dropped the
   stale decode-module import (commit `1889bcb`).

## Builds workflow fix (commit `b09b2c1`)
- Root cause (hard data from CI logs): both macOS DMG jobs fail the PyInstaller
  onefile self-test with `QtQuickControls2Macos.framework` "File already exists
  but should not" / "Failed to create symbolic link" (exit 255).
- Good run 37978873053 (v4.1.0, 26 days ago): PyQt6 6.11.0 + PyQt6-Qt6 6.11.2 +
  PyInstaller 6.22.3 — all six jobs green.
- Failed run 41934681262: PyQt6 6.12.0 + PyQt6-Qt6 6.12.0 + PyInstaller 6.22.3 —
  Windows/Linux green, both macOS jobs failed. The only changed packaging input is
  the PyQt6-Qt6 wheel generation (6.11.2 → 6.12.0), whose new Qt framework layout
  triggers the known PyInstaller .framework symlink-duplication failure class.
- Fix: `requirements-launcher.txt` pins `PyQt6>=6.6,<6.12` and
  `PyQt6-Qt6>=6.6,<6.12` (single source for all three platform jobs).

## Verification (hard data)
- `cargo check/build --workspace --release` → Finished, no errors
- `cargo test --workspace --release` → 5 passed, 0 failed (upstream removed its lib
  tests in 88fcd87; the fork's metadata sidecar tests survive)
- `tape-decode list-profiles` → all 70 profiles incl. SECAM_QUADRUPLEX, SECAM_TYPEC,
  MESECAM_*, 405_BETAMAX, 819_QUADRUPLEX
- Real-data SECAM Quadruplex decode (etienne colorbars 50f FLAC,
  SECAM_QUADRUPLEX): **399 fields in 54.2s**, CVBS warn fired for `--chroma-out`
  (ignored, single combined .tbc, no chroma file), sidecar: system=SECAM,
  tapeFormat=QUADRUPLEX, rfSourceSampleRate=40000000.0, gitCommit=677b80f (rebased)
- Real-data PAL S-VHS decode (EBU bars 16 frames, PAL_SVHS): **33 fields in 13.2s**
  — matches the documented pre-rebase result (33 fields, ~12s), sidecar intact
- Profile round-trip (write-profile + JSON check): SECAM_QUADRUPLEX/SECAM_TYPEC
  direct=True cvbs_single=True; SECAM_VHS (method-1) and MESECAM_VHS direct=False
  cvbs_single=False; NTSC_TYPEC direct=False cvbs_single=True (is_composite_color)
  — identical to the 20261102 round's validated matrix
- MESECAM_VHS decode of `mesecam_vhs_tape275_testcard_50f.flac`: 0 fields — **A/B
  verified NOT a regression**: the pre-rebase backup binary (built in a worktree)
  produces the identical 0-field result with the same args (sample/profile
  mismatch, FLAC header rate 17.9 kHz; needs a different profile/frequency than
  MESECAM_VHS default)
- `QT_QPA_PLATFORM=offscreen python3 decode_selftest.py` → **SELFTEST OK**

## Tags repointed (lightweight, all matched uniquely by subject)
v, v2.0.0b → 36a1d6d; v1.0.0 → 0c5223f; v2.0.0 → 8fd41a2; v3.0.0 → 4c4f8d9;
v4.0.0 → 79bcf32; v4.0.1 → 2a1b94a; v4.1.0 → 2194be2

## Push + CI + release
- `git push --force-with-lease fork master` + force-push repointed tags + backup branch
- CI: "Build and release binary" workflow_dispatch (no release) — all six jobs must
  be green (macOS selftest must print SELFTEST OK with the pinned PyQt6)
- Release: dispatch with create_release=true, release_tag=v4.2.0
