# Incremental Rebase from Upstream (tape-decode-rs namazso) - Prompt Log

**Started:** 2026-11-02 22:33 UTC
**User request:** pull from upstream and rebase
**Rule applied:** prompt readme.md with all output, input, and commands run.

## Pre-rebase state (hard data)
- HEAD: `9e21e95` (`master`, `fork/master`) — in sync with fork, working tree clean (tracked files)
- Previous rebase: 20261101 (see REBASE_UPSTREAM_20261101.md) — master already based on `c0c80ce`
- Upstream fetch: `c0c80ce..3278b6f` → **30 new upstream commits**, local **63 commits** ahead
- Merge base: `c0c80ce`

## New upstream commits (30)
3278b6f decode: drop the per-line plumbing of usectoinpx
626eddd decode: take the picture as exact lines in picture_line_level
7cad050 decode: compare the second-previous field's order only when it exists
5f698de decode: keep only what the field-order check reads of past fields
86e17ed demodblock: produce the burst channel only when chroma processing reads it
05de7db chroma: clamp line ranges to the field instead of wrapping them
5d9fb2a field: key the track IRE0 offset on the field's own burst lock
1145391 decode: never drop tape input ahead of a segment still to be spawned
36a098b sync: keep the line locations in f64
98db73e spec: take the black pedestal from the line standard's colour system
17e4ceb Update base commit (no code changes)
9db31ff CLAUDE.md: state the vectorization preference order
8744e93 scale_field: resample through portable SIMD instead of x86 intrinsics
19ba88c field: resample only the burst windows for the burst lock
d651e2f decode: drop the resampler arguments every caller left at their defaults
(+ 15 more, full list in git log)

## Conflict-risk areas (from commit subjects)
- `86e17ed` demodblock burst channel gating — interacts with our is_secam_direct demod_fft branch
- `19ba88c` field burst-window resampling — interacts with our SECAM burst-lock skip
- `5d9fb2a` field track IRE0 offset keying — interacts with our ire0 back-porch/adjust work
- `98db73e` spec black pedestal — spec.rs (our secam_direct/chroma band regions)
- `05de7db` chroma line range clamping — chroma.rs (our direct dispatch region)

## Execution log

### Step 1 (22:33 UTC): fetch + prompt log (above). Backup branch `backup/pre-upstream-rebase-20261102` at 9e21e95; fork/master still at 9e21e95 = remote backup (no new zip/bundle needed for this incremental round; yesterday's RESTOREPOINT_PRE_REBASE_20261101.zip/.bundle remain in history).

### Step 2: Rebase `git rebase origin/master` (plain — history is linear since yesterday)
Stops and resolutions:
1. `61a34d0` (7eed12c replay) README clarity — resolved: fork wording + **upstream's new port-base hash 6cfd37b5** (upstream 17e4ceb bumped the base commit; the README's cut-off claim describes upstream's decoder core, so it tracks their number).
2. `5757387` (5fc9596 replay) SECAM direct-composite — **converged with upstream's 86e17ed/98db73e reworks**:
   - `demodblock.rs`: took upstream's new early burst-channel production (gated on `spec.rf_write_chroma`, gain `chroma_burst_block_fft_gain`); the fork's whole late burst-chain block — including the `is_secam_direct`/`demod_fft` branch and `chroma_afc_enabled()` raw-RF path — **dropped as obsolete**: for direct-composite `rf_write_chroma` is false (the CVBS gate), so no burst channel is produced at all, which is exactly what the CVBS single-.tbc design wants and saves the wasted per-block band-pass.
   - `spec.rs` (553-580): dropped the fork's `chroma_filter_video_burst` re-add — upstream replaced that SOS filter with `chroma_burst_block_fft_gain` (built from chroma_bpf_lower/upper + notches, gated `rf_write_chroma && !do_cafc`); the field no longer exists in the struct.
   - `spec.rs` (186-209): kept BOTH upstream's new `black_ire` pedestal block (98db73e) AND the fork's `secam_direct` flag + direct-composite CAFC bail (old simple definition at this point in the replay; the CVBS commit later refines it to the carrier-at-block-centre definition).
   - Compile check passed before staging.
3. `8bd89be` (fe11842 replay) README image — resolved: fork's "A FM RF --> 4fsc .tbc (CVBS/S-Video) decoder..." wording + hash 6cfd37b5 (fixed a duplicated `## Installation` heading my first edit introduced).
4. Everything else — Formatting, docs, the CVBS commit replay, test fix, logs, restore-point commit — replayed **clean**.

Result: master = 30 upstream + 63 fork commits; old pre-rebase history on `backup/pre-upstream-rebase-20261102` and `fork/master`.

### Step 3: Tags repointed (lightweight, all matched uniquely by subject)
- `v`, `v2.0.0b` → 8001239 ; `v2.0.0` → 8bd2b64 ; `v3.0.0` → d6791d8 ; `v4.0.0` → 812119a

### Step 4: Verification (hard data)
- `cargo build --workspace` → Finished, no errors/warnings
- Key semantics survived: `spec.rs:200` `secam_direct = decoder_params.is_secam_direct(rf_color_system)` (CVBS-final definition); `spec.rs:307` `rf_write_chroma = is_color_under && !secam_direct && !rf_export_raw_tbc`; chroma.rs direct dispatch intact
- `cargo test --workspace` → **5 passed, 0 failed**
- Profile round-trip: SECAM_QUADRUPLEX direct=True cvbs_single=True; SECAM_TYPEC direct=True cvbs_single=True; SECAM_VHS direct=False cvbs_single=False (method-1 exclusion intact)
- `QT_QPA_PLATFORM=offscreen python3 decode_selftest.py` → **SELFTEST OK**

### Pending
- Push `--force-with-lease fork master` + tags + backup branch (awaiting user go-ahead)
- Optional: AppImage rebuild against the new tree; real-data SECAM decode A/B
