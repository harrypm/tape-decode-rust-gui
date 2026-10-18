# CVBS formats single combined .tbc output (SMPTE Type C / Quadruplex) - 2026-10-18

Task: "fix cvbs formats to have single .tbc combined output i.g SMPTEC and QUAD".

Problem: composite (CVBS) formats - the format bases with `is_composite_color: true`
(QUADRUPLEX, TYPEC, TYPEB, VHD - i.e. the `*_QUADRUPLEX` / `*_TYPEC` / `*_TYPEB` / `*_VHD`
profiles) - demodulate the whole composite into the luma `.tbc` (downstream decodes colour
from that single combined file). But `--chroma-out` was still honoured by the CLI: the
chroma file was opened, the decoder produced no chroma channel for these formats, and the
first field write failed mid-decode with "missing chroma output for chroma file", leaving
partial outputs. The GUI passes `--chroma-out` by default whenever its (default-checked)
"Write chroma output (_chroma.tbc)" checkbox is checked, so every guided SMPTEC/QUAD
decode failed.

Direct-composite SECAM profiles (SECAM_QUADRUPLEX / SECAM_TYPEC, `is_composite_color:
false` by design) keep their separate `_chroma.tbc` output (SECAM-direct path) and are
NOT affected by this change.

## Files changed

- crates/tape-decode-cli/src/cli.rs
  - `run_decode`: captures `composite_color = profile.decoder_params.is_composite_color`
    before the profile moves into the request. For composite profiles `--chroma-out` is
    ignored with a `WARN` ("Composite-colour (CVBS) profile: the luma output contains the
    combined composite signal, so --chroma-out is ignored and a single .tbc is written")
    and no chroma file is opened -> single combined `.tbc` + `.tbc.json`, exit 0, no
    stray `_chroma.tbc`.
  - `run_write_profile`: `--out -` now writes the flattened profile JSON to standard
    output (used by the GUI/Python to read `decoder_params.is_composite_color`).
- decode_runtime.py
  - New `profile_is_composite(profile=..., profile_file=...)` -> True/False/None via
    `tape-decode write-profile [--profile NAME | --profile-file FILE] --out -`;
    None = unknown (binary missing / bad profile / bad JSON); absent
    `is_composite_color` key defaults False (serde default, e.g. PAL_VHS).
- decode_launcher.py
  - Composite (CVBS) profile detection state (`_profile_composite`, remembered user
    checkbox state, per-embedded-name query cache) + a 250 ms single-shot debounce
    QTimer (`_schedule_composite_check` / `_run_composite_check`), re-checking on
    profile combo change, profile-file toggle/text, and after `_refresh_profiles`
    (addItems run under blockSignals).
  - `_refresh_tool_state`: chroma checkbox handled separately
    (`_apply_chroma_check_state`); for CVBS profiles it is unchecked+disabled with a
    tooltip, and the note label appends "CVBS/composite profile: single combined .tbc
    output...". The user's checked/unchecked choice is remembered and restored when
    switching back to a colour-under profile.
  - `_build_decode_args`: never passes `--chroma-out` when the current profile is
    composite (guard independent of the checkbox state).
- decode_selftest.py
  - New step 3: CVBS detection through the bundled binary - a non-SECAM QUAD/TYPEC
    profile from `list-profiles` must read back composite=True, a VHS profile
    composite=False.
- README.md
  - write-profile help block refreshed to the real CLI (`--out <OUT>` option, `-` =
    stdout; the old block showed a stale positional `<OUT>`).
  - New example section "Composite (CVBS) formats write a single combined `.tbc`"
    with the PAL_QUADRUPLEX command + the warning text, and the SECAM-direct exception.

## Commands run (build/test)

- cargo build --release --bin tape-decode  -> Finished (1m23s; one intermediate fix:
  struct brace + capture `composite_color` before the profile move)
- cargo test --release                   -> 25 passed, 0 failed (20 tape-decode + 5 cli)
- cargo fmt --check                      -> cli.rs clean; flagged diffs are pre-existing
  in spec.rs/build.rs (older rustfmt) - not touched

## Verification (hard data)

write-profile stdout (all 70 profiles checked via decode_runtime.profile_is_composite):
- composite=True exactly: 819_QUADRUPLEX, NTSC_TYPEB, NTSC_TYPEC, NTSC_VHD,
  PAL_QUADRUPLEX, PAL_TYPEB, PAL_TYPEC
- SECAM_QUADRUPLEX / SECAM_TYPEC = False; PAL_VHS = False; none unknown

Real data (ffprobe first: flac, header 40000 Hz = 40 MHz RF convention, s16 mono,
21.75M samples ~ 0.54 s):
/home/harry/Desktop/SECAM/etienne_Quadruplex_colorbars-20260824_SECAM_MISRC2.5_16bit_40mhz-cut.flac

1. PAL_QUADRUPLEX + `--chroma-out ..._chroma.tbc`: exit 0, WARN logged, 26 fields,
   single quad-pal.tbc 18473260 bytes = 26*1135*313*2 (exact u16 field geometry),
   quad-pal.tbc.json sidecar, NO _chroma.tbc on disk.
2. PAL_TYPEC (SMPTE Type C): same - exit 0, WARN logged, 26 fields, single .tbc,
   no _chroma.tbc.
3. SECAM_QUADRUPLEX regression + `--chroma-out`: exit 0, no WARN, luma .tbc AND
   _chroma.tbc both written (18473260 bytes each) - SECAM-direct unchanged.

Outputs kept in /tmp/cvbs_check/ (incl. gui_offscreen_test.py).

GUI (offscreen, QT_QPA_PLATFORM=offscreen, .venv-launcher python; 28 checks):
- PAL_VHS: chroma checkbox enabled+checked, --chroma-out in built args
- PAL_QUADRUPLEX / PAL_TYPEC: composite=True resolved, checkbox disabled+unchecked,
  tooltip set, note shows CVBS, --chroma-out NOT in built args, preview has no
  _chroma.tbc; switch back to PAL_VHS re-enables and restores the user's choice
  (both the checked and unchecked memory cases)
- SECAM_QUADRUPLEX: checkbox enabled, --chroma-out passed (kept)
- profile-file mode (flattened PAL_QUADRUPLEX JSON via write-profile --out -):
  composite detected, checkbox disabled, no --chroma-out
- debounce timer wiring resolves composite without direct calls

Selftest: `.venv-launcher/bin/python decode.py --selftest` -> SELFTEST OK, exit 0.

## Round 2 (2026-10-18): SECAM-direct profiles also single combined .tbc

User feedback after a real GUI decode of the full tape (SECAM_QUADRUPLEX): "its not
making a single .tbc cvbs file ... It still wrote two .tbc files" - i.e. SECAM QUAD
was expected to get the single combined CVBS output too, but the first round kept the
SECAM-direct separated path (luma + _chroma.tbc).

Changes:
- profiles.json: SECAM_QUADRUPLEX drops its 3.2 MHz/order 12 video_lpf override
  (inherits the QUADRUPLEX base full-band luma chain, same as PAL_QUADRUPLEX);
  SECAM_TYPEC video_lpf 3.2 MHz/order 12 -> 5.2 MHz (matches PAL_TYPEC). The dots-
  suppressing LPF belonged to the separated mode; combined CVBS keeps the chroma
  block in the composite by design.
- crates/tape-decode/src/request.rs: new `DecoderParams::is_secam_direct(color_system)`
  - SECAM colour system + no chroma_carrier_mult + color_under_carrier at the SECAM
    block centre (4328125 Hz +-1 kHz). The old `Secam && no carrier_mult` predicate
    wrongly caught MESECAM_VHS (SECAM colour system, block at the VHS colour-under
    carrier 732.4 kHz) - a latent misrouting since the direct-composite commit.
- crates/tape-decode/src/spec.rs: secam_direct via the new predicate; rf_write_chroma
  now false for secam_direct (single combined .tbc, no separate chroma channel);
  DecoderSpec::is_secam_direct() mirrors the same definition on the built spec.
- crates/tape-decode/src/decode/mod.rs: re-export SECAM_BLOCK_CENTRE.
- crates/tape-decode-cli/src/cli.rs: cvbs_single_output = is_composite_color ||
  is_secam_direct -> --chroma-out ignored with the CVBS warning (single .tbc).
- decode_runtime.py: profile_is_composite mirrors the predicate from the flattened
  profile JSON (is_composite_color, or SECAM + no carrier_mult + carrier at block
  centre); MESECAM_VHS correctly False.
- decode_selftest.py / GUI: same rules; the GUI now disables the chroma checkbox for
  SECAM_QUADRUPLEX / SECAM_TYPEC too (auto enable/disable for CVBS vs S-Video style
  output formats, both directions, profile names and profile files).

Verification:
- cargo test --release: 25 passed. Profile sweep: SECAM_QUADRUPLEX/SECAM_TYPEC/
  PAL_QUADRUPLEX/PAL_TYPEC/NTSC_TYPEC/819_QUADRUPLEX/PAL_TYPEB/NTSC_VHD = True;
  SECAM_VHS / MESECAM_VHS / PAL_VHS = False.
- Cut decode SECAM_QUADRUPLEX + --chroma-out: exit 0, CVBS warning, single .tbc
  (18473260 = 26x1135x313x2 exact), no _chroma.tbc, sidecar system=SECAM
  tapeFormat=QUADRUPLEX.
- Spectrum check (line 100 FFT): 4.1-4.6 MHz chroma-block band energy vs 0-3 MHz
  luma = 0.264 (38.9x the old dots-filtered luma's 0.0068) - the block is IN the
  single .tbc.
- decode.py --selftest -> SELFTEST OK; offscreen GUI test (30 checks) OK.
- USER-CONFIRMED (2026-10-18): GUI decode produces the single combined .tbc; chroma
  checkbox auto-disables for CVBS profiles.

Restore point: RESTOREPOINT_CVBS_SINGLE_TBC_20261018.zip (repo root, all files
changed in rounds 1-2).

## Round 3 (2026-10-18): luma dots fixed on the chroma-decoder side

User: "lets fix this either on the chroma-decoder or the decode side". The dots
cannot be removed from the combined .tbc itself (the block IS the colour in a CVBS
signal), and the decode side must keep the composite intact - so the fix belongs
where the dots actually surface: tbc-chroma-decoder's Y output.

Root cause (tbc-tools/src/tbc-chroma-decoder/secamdecoder.cpp):
`SecamDecoder::decodeFrames` wrote the output luma as a raw passthrough of the input
line ("composite passthrough"). Fine for split-chroma input (no luma in the file;
real luma comes from the separate luma TBC), but for a combined input the SECAM FM
block rode straight into the video as dots. The PAL decoder never had this problem
(palcolour computes luma as composite minus demodulated chroma).

Fix (SecamDecoder only, gated on the existing combinedInput detection):
- FieldWork gains `maskLuma` and `lumaFiltered`.
- New `filterRowLuma()`: same block-FFT context scheme as filterRowAnalytic, real
  symmetric gain, zero-phase result. Run over all rows in decodeField's pass loop
  when combinedInput (re-runs after a mask recentre).
- decodeFrames Y loop: combined input -> outY = lumaFiltered; split chroma ->
  unchanged passthrough.
- Round 3a (first attempt) used maskLuma = 1 - bandGain (a notch around the steady
  block, +-580 kHz). User re-check: "Dots issue is still heavy ... The dots are on
  the transitions between colours" - correct: at a colour edge the FM carrier
  sweeps the full deviation and its transient sidebands spread across the band
  skirts, which a steady-block notch cannot catch.
- Round 3b (final): maskLuma is a proper luma LOW-PASS below the FM block floor -
  the receiver-standard SECAM luma path. Raised cosine 3.45 -> 3.75 MHz (the
  combined band's stop floor, centre+offset-580 kHz), gain 1 below 3.45, 0 above
  3.75. Kills steady-state carriers AND colour-edge sidebands; luma above 3.75 MHz
  is chroma-dominated in a CVBS source and unrecoverable anyway. The raw .tbc still
  carries the full composite (colour intact for the U/V demod).

Verification (combined cut decode /tmp/cvbs_check/v4/secam-quad-v5.tbc + json):
- Rebuilt tbc-chroma-decoder (nix develop -c ninja -C build): all 33 ctest tests
  pass, incl. decode-pretbc-secam-etienne-cbars and both MESECAM fixtures.
- -p yuv decode: SECAMINPUT combined=1 on all 26 fields; YUV444P16 928x576 x13.
- Notch attempt (3a): block band 4.1-4.6 MHz dead (143) but transition sidebands
  remained (622k in 3.7-4.1, 625k in 4.6-5.5) -> user-visible dots on colour edges.
- LPF fix (3b): mean block+sideband energy above 3.75 MHz over picture lines =
  0.00143 of luma (frames 0 and 6) vs 0.264 in the raw combined .tbc - steady
  carriers and colour-transition sidebands both gone; 0-3 MHz luma untouched.
- Frames for visual check: /tmp/cvbs_check/v4/lpf_frame{0,6}.png (bar crop),
  lpf_frame0_full.png (full frame); USER CONFIRMATION of the picture pending.
- User's PATH pointed at the fixed build: ~/.local/bin/{tbc-chroma-decoder,
  tbc-analyse,tbc-video-export} -> tbc-tools/build/bin (matches the repo's local
  symlink convention; ~/.local/bin precedes ~/.nix-profile/bin in PATH, so the
  stale nix-profile copy no longer shadows the fix).

## Side note: raw .tbc luma dots (expected for combined CVBS)

The single combined .tbc itself still shows the block as dots in ld-analyse - that
is inherent to CVBS (the raw capture signal) and unchanged by this fix; the decoded
video/export is where luma must be clean, and now is.

## Not yet done / next steps
- AppImage/PyInstaller bundle + release rebuild would follow the round-5 recipe in
  PROMPT_secam_direct_profiles_20261008.md if wanted.
- No real NTSC/PAL SMPTE Type C or Quadruplex sample beyond the SECAM quad tape was
  available for a colour-quality check; decodes verify output structure (single
  combined .tbc, geometry, sidecar, block energy), not picture content.
- MESECAM_VHS chroma routing was corrected to the generic colour-under path as a
  side effect of the predicate fix (master routed it to the direct-composite block
  band, which cannot find a 732 kHz block); not verified against a real MESECAM
  tape - flagged for a future pass.
