# Rebase and Update from Upstream (tape-decode-rs namazso) - Prompt Log

**Started:** 2026-11-01 21:19 UTC
**User request:** Preserve my SECAM works and metadata updates and other little changes; merge upstream tape-decode changes into my repo — a clean rebase.
**Rule applied:** When a prompt is started, a prompt readme.md should be made on the host system with all output, input, and commands run.

## Remotes
- `origin` (upstream): https://github.com/namazso/tape-decode-rs.git
- `fork`: https://github.com/harrypm/tape-decode-rust-gui.git

## Pre-rebase state (hard data)
- HEAD: `a16ccba` (`master`, `fork/master`) — docs(logs): record SECAM profiles v4.1.0 release round
- Divergence since merge-base `93c9788` (93c97886f7e570970dd71f43cdd649360294d30d): **64 local commits** vs **35 upstream commits**
- `git cherry origin/master master`: **0 patch-equivalent commits** — upstream reworked the fork's PR #1 (secam-vhs-profile) and PR #4 (secam-clamp-and-block-anchored-bpf) contributions; nothing replays automatically.
- Baseline verification 2026-11-01 21:47 UTC:
  - `cargo check --workspace` → exit 0 (`Finished dev profile ... in 12.81s`)
  - `python3 -m py_compile decode_launcher.py decode_runtime.py decode_selftest.py` → OK

## Fork-only work that must be preserved
- `5fc9596` Add SECAM direct-composite decoding (2" Quad / SMPTE Type C) - R1
- `0cfc949` CVBS formats: single combined .tbc output + GUI auto chroma checkbox
- `981942e` feat(metadata): record rfSourceSampleRate in the decode JSON sidecar
- Qt6 GUI / launcher / AppImage / cross-platform CI packaging chain (starting `4d56cbe`)
- Docs + prompt_logs commits

## Upstream-absorbed work (drop on rebase, upstream version wins)
- `632498c` feat(secam): SECAM method 1 VHS decoding (upstream: `d817f3e`, `fa28f7b`, reworked 552-line secam.rs)
- `a16a744` feat(secam): clamp under-carrier deviation to SECAM-legal corridor (upstream: `6415608`)
- Merge commits `b7a34a1` (PR#1) and `63c5265` (PR#4) from hugoatease — content now upstream

## Files touched by both sides (conflict candidates)
cli.rs, metadata.rs, profiles.json, sys_params.json, writer.rs, chroma.rs, demodblock.rs, field.rs, decode/mod.rs, secam.rs, request.rs, spec.rs, README.md

## Range particulars
- Merge commits in range: `63c5265`, `b7a34a1` → rebase uses `--rebase-merges`
- Tags in range: `v`, `v2.0.0`, `v2.0.0b`, `v3.0.0`, `v4.0.0` → repoint after rebase
- Upstream tip at fetch: `c0c80ce` (Add CLAUDE.md)

## Upstream diff summary (93c9788..c0c80ce)
23 files, +3329/-35056 (removes two 16k-line subdeemphasis LUT txts; rewrites sync.rs; new reworked secam.rs 552 lines; profiles/format/sys params expanded incl. MESECAM*/SECAM_VHS_* profiles)

## Execution log

### Step 1 (21:47 UTC): prompt log created; baseline captured above.

### Step 2 (22:13 UTC): Restore point
- Branch `backup/pre-upstream-rebase-20261101` at `a16ccba`
- `RESTOREPOINT_PRE_REBASE_20261101.bundle` (all refs, 2.4 MB) + `RESTOREPOINT_PRE_REBASE_20261101.zip` (source tree, 722 KB)
- `fork/master` also still holds the full pre-rebase history remotely

### Step 3 (22:20–23:0x UTC): Rebase `git rebase --rebase-merges origin/master`
Stops and resolutions (in replay order):
1. `632498c` SECAM method-1 VHS — **skipped**: profiles.json/sys_params.json sets verified identical to upstream (upstream additionally has PAL_UMATIC_SP); reworked colour-under implementation is upstream.
2. `a16a744` under-carrier clamp — **skipped**: upstream `secam.rs` applies the same BT.470 corridor clamp with f64 refinement (lines 122–140) + CARRIER_RANGE_HZ clamp.
3. `7eed12c` README clarity — resolved: fork wording + upstream's newer port-source commit ref (fe3f6099) + kept GUI section.
4. `5fc9596` SECAM direct-composite 2" Quad / SMPTE Type C R1 — **ported onto upstream's rework**:
   - `spec.rs`: kept upstream's CAFC warn-and-ignore for colour-under SECAM; kept fork's hard bail for direct-composite + CAFC (moved before the warn so it stays reachable); `secam_direct` flag; `SECAM_BLOCK_BAND` import; direct block-band arm in `chroma_bandpass_final`; `chroma_filter_video_burst` direct branch (auto-merged); `is_secam_direct()` method; **dropped `level_detect_divisor` validation** (upstream removed the feature entirely; no remaining references in tree).
   - `secam.rs`: resolved to upstream side wholesale (`git checkout --ours`), then re-added `SECAM_BLOCK_CENTRE`/`SECAM_BLOCK_BAND` consts and ported `process_chroma_secam_direct` adapted to upstream conventions (Vec<f32> return + `encode_chroma` in chroma.rs, `analytic_signal`, `.expect()` style, `burst_area` -> `(usize, usize)`) plus `median_of` helper. Fork's superseded method-1/flywheel/encode_chroma_u16 dropped.
   - `chroma.rs`: direct dispatch inserted after `tracked_chroma`, before the ME-SECAM carrier servo.
   - `field.rs` (auto-merged): geometry-derived `ire0_backporch` window + SECAM burst-lock skip, composing with upstream's `refine_levels_from_vsync` + `rf_ire0_adjust` (backporch/hsync).
   - `request.rs` (auto-merged): fork's profile-level `ire0_adjust: bool` kept as the profile switch.
   - `cli.rs`: upstream's multi-mode `--ire0-adjust [backporch|hsync]` kept (bare flag still selects backporch, so the Python launcher needs no change) + fork profile fallback: `backporch |= profile.decode_options.ire0_adjust`.
   - `secam_analytic_fft` build condition extended with `|| secam_direct` (else the direct path would panic on the missing transform).
   - Compile check passed before staging.
5. `fe11842` README image example — resolved (fork's examples taken).
6. `4aea2ff` Formatting, `879058d` docs — replayed clean.
7. `0cfc949` CVBS single combined .tbc + GUI auto chroma — **ported**:
   - `cli.rs`: `cvbs_single_output` block + upstream's `let mut request`.
   - `spec.rs`: import union (added `SECAM_BLOCK_CENTRE`); `secam_direct = decoder_params.is_secam_direct(rf_color_system)` (fork's final definition: carrier-mult None AND declared carrier at the SECAM block centre — excludes MESECAM and method-1); `rf_write_chroma` = upstream's processing gate + `&& !secam_direct`; upstream's `chroma_output` write gate and `burst_abs_ref` bail kept.
   - `request.rs`/`mod.rs`/`chroma.rs`(spec method)/`profiles.json` auto-merged: `DecoderParams::is_secam_direct`, `SECAM_BLOCK_CENTRE` re-export, spec method with centre check, SECAM_QUADRUPLEX/SECAM_TYPEC profile entries with `color_under_carrier: 4328125.0` (the block centre).
   - Compile check passed before staging.
8. `a16ccba` docs — replayed clean. **Rebase completed successfully.**

Result: `master` = 35 upstream commits + 60 fork commits (64 − 4 skipped/absorbed) + 1 test fixup. Old pre-rebase commits remain reachable via `backup/pre-upstream-rebase-20261101` and `fork/master` until force-push.

### Step 4: Tags repointed (lightweight)
- `v`, `v2.0.0b` → c37c614 (fix(ci): dedupe linux appimage packaging outputs)
- `v2.0.0` → 485c979 (AppImage glibc-2.35 restore point log)
- `v3.0.0` → e6a3700 (Windows arm64 build naming)
- `v4.0.0` → d63132f (metadata Medium/Format/System)

### Step 5: Verification (hard data)
- `cargo build --workspace` → Finished, no errors/warnings (23.28s)
- `tape-decode list-profiles` → SECAM_QUADRUPLEX, SECAM_TYPEC, all SECAM_VHS*/MESECAM_VHS*, 819_QUADRUPLEX, PAL_UMATIC_SP (upstream's new one) present
- `tape-decode decode --help` → `--ire0-adjust [<IRE0_ADJUST>]`, "a bare flag selects backporch" (Python launcher's bare `--ire0-adjust` remains valid)
- `python3 -m py_compile decode.py decode_launcher.py decode_runtime.py decode_selftest.py` → OK
- Profile round-trip via `write-profile --out -`:
  - SECAM_QUADRUPLEX: color_system=SECAM carrier=4328125.0 direct=True → cvbs_single=True
  - SECAM_TYPEC: direct=True → cvbs_single=True
  - PAL_QUADRUPLEX: is_composite=True → cvbs_single=True
  - PAL_VHS: carrier=626953.0 → cvbs_single=False
  - SECAM_VHS: carrier=1082031.25 (= block centre / 4, method-1 divide-by-4) → direct=False → cvbs_single=False (MESECAM/method-1 exclusion verified)
- `cargo test --workspace` → initially failed to compile (test-only): upstream's new `blanking_16b_ire` field missing from fork's `dummy_decoder_metadata`; fixed in commit 33efa27 → **5 passed, 0 failed**

### Step 5b: Headless bundle self-test
`QT_QPA_PLATFORM=offscreen python3 decode_selftest.py` → **SELFTEST OK** (PyQt6 offscreen QApplication, tape-decode binary resolve + list-profiles, CVBS single-output detection via write-profile). Style-override warning is cosmetic (desktop theme unavailable offscreen).

### Step 6 (01:30–01:38 UTC): AppImage rebuild + bundle self-test
- `cargo build --release` → Finished (1m10s); fresh binary at `target/release/tape-decode` (Nov 2 01:30)
- Packaging gotcha caught: `build-linux-decode-bin.py` auto-picked a STALE pre-rebase binary (`target/x86_64-unknown-linux-gnu/release/tape-decode`, Oct 9 20:00). Removed it and re-bundled with `TAPE_DECODE_BIN=target/release/tape-decode` (log line confirms "Bundling default .../target/release/tape-decode").
- PyInstaller bundle rebuilt (Qt plugins, libglvnd GL libs added) → AppDir assembled per `build_linux_decode.yml` → `decode-rust-gui-linux_dev-33efa27_x86_64.AppImage` (152 MB)
- `TAPE_DECODE_MICROARCH=x86-64-v1 QT_QPA_PLATFORM=offscreen APPIMAGE_EXTRACT_AND_RUN=1 ./decode-rust-gui-linux_dev-33efa27_x86_64.AppImage --selftest` → **SELFTEST OK** (bundle-internal PyQt6 + Qt platform plugin + fresh rebased tape-decode binary + CVBS detection)
- Note: new artifacts (AppImage, RESTOREPOINT_PRE_REBASE_20261101.zip/.bundle) left untracked pending user confirmation per restore-point rule.

### Not yet verified (needs user / real data)
- GUI launcher real-world behaviour (auto-chroma checkbox, CVBS single .tbc flow) — requires user confirmation per rules
- Real SECAM Quad/Type C / ME-SECAM decode A/B against pre-rebase output — needs a sample RF capture path
- Push `--force-with-lease` to fork/master + tag push (awaiting user go-ahead)

(AppImage rebuild + `--selftest` completed in Step 6 above.)
