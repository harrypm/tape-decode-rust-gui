# SECAM direct-composite profiles (SMPTE Type C / Quadruplex) - 2026-10-08

Task: add SECAM profiles for direct-composite (2" Quad, 1" SMPTE Type C) tapes, where the
whole SECAM composite FM-modulates the tape and the chroma block (foB 4.25 / foR 4.40625 MHz)
comes out of the demodulator inside the composite. The problem: the QUADRUPLEX/TYPEC format
bases force `is_composite_color: true` (no chroma channel at all, downstream decodes from the
combined composite - luma energy inside the chroma block demodulates as "luma dots"), and the
PAL profile route gives "a usable image but no proper colour". This adds a real SECAM
direct-composite path: luma filtered OUT of the chroma (SECAM block band-pass on the
demodulated composite) and chroma filtered OUT of the luma (~3.4 MHz luma LPF).

## Files changed
- crates/tape-decode/src/decode/secam.rs
  - SECAM_FOR/SECAM_FOB made pub(crate); added SECAM_BLOCK_CENTRE (4.328125 MHz) and
    SECAM_BLOCK_BAND (centre-670k .. centre+550k Hz, same shape as the method-1 band).
  - New `process_chroma_secam_direct()`: final block-band pass at the TBC output rate,
    per-field level normalisation from the median carrier envelope (target = method-1
    porch level, burst_abs_ref*sqrt2; SECAM has no burst, per-line AGC would flatten the
    foR/foB rest-amplitude difference), blank vertical interval below line 16, encode u16
    (zero at 32767). Carrier collapses (dropouts/mono) pass through as the squelch.
- crates/tape-decode/src/decode/mod.rs
  - Re-export SECAM_BLOCK_BAND; `metadata()` system now reports "SECAM" for SECAM.
- crates/tape-decode/src/spec.rs
  - `secam_direct` flag (SECAM colour system + no chroma_carrier_mult). CAFC rejected for
    SECAM-direct. chroma_filter_video_burst = SECAM block band at the block/demod rate;
    chroma_bandpass_final None-arm anchors on the SECAM block band. `is_secam_direct()`.
- crates/tape-decode/src/decode/demodblock.rs
  - Burst-chain source for SECAM-direct is the demod spectrum (demod_fft) instead of raw
    RF: the chroma block lives in the demodulated composite, so the |H|^2 SECAM band gain on
    the demod is what filters the luma out of the chroma.
- crates/tape-decode/src/decode/chroma.rs
  - Dispatch: SECAM + no carrier_mult -> process_chroma_secam_direct (skips the
    burst-locked heterodyne path entirely).
- crates/tape-decode/src/decode/field.rs
  - apply_burst_lock skipped for SECAM (no burst; the phase sequence only feeds the
    heterodyne up-conversion both SECAM paths skip; output-identical, saves per-line work).
- crates/tape-decode-cli/src/profiles/profiles.json
  - New `SECAM_QUADRUPLEX` (base QUADRUPLEX, is_composite_color false, chroma_offset 0,
    color_under_carrier 4328125, video_lpf 3.4 MHz/order 10, sys_params SECAM with quad
    hz_ire 12167.83/ire0 7.68 MHz, fallback_vsync) and `SECAM_TYPEC` (base TYPEC, same SECAM
    changes, mirrors PAL_TYPEC's video_bpf/hpf/deemph).

## Commands run (build/test)
- cargo build --release -p tape-decode-cli   -> Finished (1m16s)
- cargo test --release                      -> 25 passed, 0 failed
- ./target/release/tape-decode list-profiles -> SECAM_QUADRUPLEX / SECAM_TYPEC listed

## Test decode (real data)
Input: /home/harry/Desktop/SECAM/etienne_Quadruplex_colorbars-20260824_SECAM_MISRC2.5_16bit_40mhz-cut.flac
(ffprobe: flac 40000 Hz header = 40 MHz RF convention, s16 mono, ~0.54 s ~ 26 fields)

    ./target/release/tape-decode decode --profile SECAM_QUADRUPLEX --frequency 40 \
      --input-format flac --debug --overwrite \
      --luma-out /tmp/secamdirect/etienne-cut-secamquad.tbc \
      --chroma-out /tmp/secamdirect/etienne-cut-secamquad_chroma.tbc \
      --metadata-out /tmp/secamdirect/etienne-cut-secamquad.tbc.json <cut.flac>

Result: 26 fields decoded (1.87 FPS), "SECAM direct chroma porch level" ~3300 per field,
sidecar system=SECAM tapeFormat=QUADRUPLEX, 1135x313, sr 17.734475 MHz.

## Verification (hard data; scripts kept in /tmp/secamdirect/)
verify_secam_direct.py:
- Per-line rest carrier over the blanking window (lines 20..39): alternates D'B/D'R;
  clusters 4.2425 MHz (7.5 kHz from nominal 4.25) and 4.3983 MHz (8.0 kHz from 4.40625).
- Envelope by region (line 100): sync ~540 (subcarrier gated during sync), porch ~5472
  (rest carrier present), active ~8510 (bell-shaped carrier).
- Active deviation (line 100, rest-referenced): min -452 kHz, median -1.6 kHz,
  max +3752 kHz, 98.4% of samples inside the legal BT.470 corridor (rest are dropout
  transients; downstream click concealment + deviation rail handle those).
- Luma chroma-band energy (3.658-4.878 MHz) vs the old Python composite luma of the same
  tape, same field/lines: -15.5 dB; core band (3.9-4.878 MHz): -16.5 dB.

End-to-end with the real downstream decoder (tbc-tools):
    /home/harry/tbc-tools/build/bin/tbc-chroma-decoder -p yuv \
      --input-json /tmp/secamdirect/etienne-cut-secamquad.tbc.json \
      /tmp/secamdirect/etienne-cut-secamquad_chroma.tbc \
      /tmp/secamdirect/secam_chroma_decoded.yuv
- SECAMDIAG all 26 fields: parity votes decisive (evenIsRed 288-291 of 291 picture lines),
  redRestMed ~4.402 MHz, blueRestMed ~4.248 MHz, 13 frames colourised at 928x576.
- Bar colours from the YUV output vs expected 75% SECAM (R-Y, B-Y): 7/8 bars match
  (white -0.01/-0.04 vs 0/0; yellow +0.04/-0.75 vs +0.09/-0.67; green -0.32/-0.48 vs
  -0.44/-0.44; magenta +0.36/+0.41 vs +0.44/+0.44; red +0.40/-0.19 vs +0.53/-0.22;
  blue -0.04/+0.66 vs -0.09/+0.66; black 0.01/0.01 vs 0/0). Cyan reads -0.41/+0.15 vs
  expected -0.67/+0.08 (right direction, ~60% magnitude - dropout damage on that band).
- Previews: /tmp/secamdirect/final_frame.png (luma weave + decoder U/V),
  /tmp/secamdirect/etienne-cut-secamquad_preview.png (numpy mini-decoder).
- Note: the -1.902 D'R polarity is absorbed by the negative Hz-per-unit constant, so the
  demodulated Dr is plain (R-Y): on the red bar the D'R carrier falls, reading positive.

## Round 2 (2026-10-09): "phase/black level a bit off"

Symptom reported by user after viewing the decode. Measurements (all from the .tbc data):

- Back porch (0 IRE) read 20100 in the first decode = +10.1 IRE over the nominal mapping:
  this machine's 0-IRE carrier is ~123 kHz above the profile's ire0 7.68 MHz (ire0_adjust
  measured 7.8002-7.8036 MHz per field, consistent).
- Second, bigger calibration error: the bar levels fit EBU 100/0/75/0 bars at 1.245x the
  profile's IRE scale -> real hz_ire ~15.2 kHz/IRE (black 7.80 MHz, white ~9.33 MHz) -
  this is Quadruplex HIGHBAND; the QUADRUPLEX format base inherited Type C's
  12.17 kHz/12167.83 geometry, so whites read +125 IRE (near-clip).
- Attempted chroma-vs-luma registration check (bar-edge midpoints): with-deemph deltas
  -17/-14/-15 px, no-deemph -16/+32/-10 px, old COMPOSITE tbc (Y and C from the SAME
  samples) -43/+31/+18 px. The single-signal scatter proves the measurement is confounded
  by FM-discriminator step skew at the band edges (direction-dependent), NOT a real
  timing error. No confirmed registration error; not fixed.

Changes:
- decode/field.rs: ire0_backporch window now derived from the standard's geometry
  (hsync_pulse_us*outfreq+8 .. active_video_us[0]*outfreq-12) instead of hardcoded
  (96,160)/(74,124) - the old hardcode landed inside the 9 us 405-line sync tip.
- request.rs DecodeOptions + cli.rs: new `ire0_adjust` decode option (profile-selectable,
  default false; CLI flag ORs with it).
- profiles.json: SECAM_QUADRUPLEX now has decode_options { fallback_vsync, ire0_adjust },
  sys_params { hz_ire 15200, ire0 7803000 } (measured quad highband); SECAM_TYPEC has
  decode_options { ire0_adjust } (geometry left at Type C nominal - no SECAM Type C
  sample to calibrate).

Commands: cargo build --release + cargo test --release (25 pass) after each change; test
decodes to /tmp/secamdirect/{adj,nodeemph,v2}; final decode with the updated profile:

    ./target/release/tape-decode decode --profile SECAM_QUADRUPLEX --frequency 40 \
      --input-format flac --overwrite --luma-out /tmp/secamdirect/v2/etienne-v2.tbc \
      --chroma-out /tmp/secamdirect/v2/etienne-v2_chroma.tbc \
      --metadata-out /tmp/secamdirect/v2/etienne-v2.tbc.json <cut.flac>

Verified result: porch 16377 (0 IRE = 16308), bars vs EBU 100/0/75/0: white 100.4,
yellow 66.3, cyan 52.5, green 43.8, magenta 31.1, red 22.3, blue 8.4, black -0.6 IRE -
all within 0.5 IRE.

## Round 3 (2026-10-09): "dots are back" on the v2 export

Measured (steady-state mid-bar chroma residue in the luma, 45 px edge margin,
p-p output units; black-white span = 37450):

  bar       v1     v2     v2 as % of span
  white      0     583     1.6   (v1 read 0 because the white bar was hard-clipped)
  yellow  2616    2185     5.8
  cyan     466     389     1.0
  green   1526    1327     3.5
  magenta  401     327     0.9
  red      881     739     2.0
  blue     401     311     0.8
  black   478     381     1.0

- v2 residue is LOWER than v1 on 7/8 bars (the 0.80x from the corrected hz_ire
  mapping plus the fixed sync levels); v1's "no dots" was partly the +10 IRE lift
  and the hard-clipped white hiding them.
- Worst leakage: yellow (D'B carrier at ~3.79 MHz, the block's low edge, where the
  3.4 MHz/order-10 LPF only gives ~-19 dB) and green. Lowering the LPF to ~3.2 MHz
  or order 12 would trade luma detail for ~10 dB less leakage - user's call.
- The chroma channels differ between v1 and v2: the sync level detection's
  thresholds and the check_levels 47-IRE blank-sync limit are derived from
  ire0/hz_ire - with the old geometry this tape's 53-IRE span FAILED the check
  (the repeated "level check failed" lines) so v1 ran on degraded sync levels;
  with hz_ire 15200 the checks pass, so v2's chroma is built on better linelocs.
- Crop of the dots: /tmp/secamdirect/v2/dots_crop.png; measurement script
  /tmp/secamdirect/v2/dots_check.py.

## Round 4 (2026-10-09): luma LPF tightened per user choice

SECAM_QUADRUPLEX and SECAM_TYPEC video_lpf 3400000/order 10 -> 3200000/order 12.
Rebuilt, re-decoded (/tmp/secamdirect/v3). Verified steady-state residue (p-p units):
white 583->174 (0.5% of span), yellow 2185->723 (1.9%), cyan 389->117, green
1327->432 (1.2%), magenta 327->98, red 739->225, blue 311->90, black 381->116
(~10 dB reduction); levels unchanged (porch 16363, white 53898 ~= 100 IRE,
black 16080).

## Round 5 (2026-10-09): GUI + AppImage rebuild for the new profiles

Local rebuild following the CI recipe (.github/workflows/build.yml, linux x86_64 job):

1. cargo build --release --target x86_64-unknown-linux-gnu --bin tape-decode
   (the target-triple path the packaging spec bundles).
2. TAPE_DECODE_BIN=target/x86_64-unknown-linux-gnu/release/tape-decode
   .venv-launcher/bin/python scripts/ci/build-linux-decode-bin.py
   -> dist/decode-rust-gui (PyInstaller onefile, bundles the new binary + the
   updated profiles.json; no stale target-x86-64-* level builds present).
3. AppDir re-assembled per CI steps; appimagetool (local
   appimagetool-x86_64.AppImage, --appimage-extract then squashfs-root/AppRun)
   -> decode-rust-gui-linux_dev-5fc9596_x86_64.AppImage (152 MB).

Verification (headless):
- dist/decode-rust-gui --selftest: SELFTEST OK, exit 0.
- dist/decode-rust-gui list-profiles: exit 0, 70 profiles, SECAM_QUADRUPLEX
  and SECAM_TYPEC present.
- AppImage --selftest (APPIMAGE_EXTRACT_AND_RUN=1, QT_QPA_PLATFORM=offscreen,
  TAPE_DECODE_MICROARCH=x86-64-v1): SELFTEST OK, exit 0.
- AppImage list-profiles: exit 0, 70 profiles, both SECAM profiles present.
- USER-CONFIRMED (2026-10-09): AppImage launched on the desktop, GUI works,
  SECAM_QUADRUPLEX and SECAM_TYPEC both listed in the profile dropdown.

## Not yet done / next steps
- SECAM_TYPEC is untested (no SECAM Type C sample in hand); parameters mirror PAL_TYPEC.
- Blanking-interval rest carrier is passed through as recorded (method-1 regenerates it
  because the divide-by-4 counter wrecks it; on this quad tape the natural rest carrier
  reads within ~8 kHz, so no regeneration was needed). Revisit if a machine shows a wrecked
  porch (regenerate_secam_blanking could be ported with carrier_mult = 1).
- Luma LPF now 3.2 MHz/order 12 (round 4); further tuning (notch at 4.286 MHz, lower
  cutoff) can be revisited against this sample or other quad material later.
- Verified dev AppImage: decode-rust-gui-linux_dev-5fc9596_x86_64.AppImage (repo root,
  built and user-confirmed 2026-10-09). CI release build still needed for a published
  release with these profiles.
