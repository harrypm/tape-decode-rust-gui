# CVBS formats: single combined .tbc output + dot-crawl fix - 2026-10-18

Hard dev note for the "cvbs formats single .tbc combined output" feature
(SMPTE Type C / Quadruplex / Type B / VHD, including direct-composite SECAM).
Full session log with all rounds: `prompt_logs/PROMPT_cvbs_single_tbc_20261018.md`.

## What changed (tape-decode-rust)

- CVBS single-output rule: composite-colour formats (`is_composite_color: true`)
  AND direct-composite SECAM (`DecoderParams::is_secam_direct`: SECAM colour
  system + no `chroma_carrier_mult` + chroma carrier at the SECAM block centre
  4328125 Hz +-1 kHz) demodulate the whole composite into the luma output, so
  `decode` ignores `--chroma-out` with a WARN and produces a single combined
  `.tbc` + `.tbc.json`. Before: the chroma file was opened, no chroma channel
  existed, and the first field write failed mid-decode ("missing chroma output
  for chroma file").
- `SECAM_QUADRUPLEX` / `SECAM_TYPEC` profiles restored to the full-band luma
  chain (the 3.2 MHz/order 12 dots-LPF belonged to the old separated mode;
  combined CVBS keeps the chroma block in the composite by design).
- `write-profile --out -` writes the flattened profile JSON to stdout.
- `decode_runtime.profile_is_composite()` resolves the output style through the
  bundled binary; the predicate is shared one-definition (`DecoderParams::
  is_secam_direct`) between the Rust decoder, the CLI gate and the Python check.
- Decode Launcher GUI auto-disables the "Write chroma output (_chroma.tbc)"
  checkbox for CVBS profiles (embedded names + profile files, debounced), with
  tooltip + note, remembering/restoring the user's choice for S-Video style
  profiles. `--chroma-out` is never built for a CVBS profile.
- Latent bug fixed en route: the old `Secam && no carrier_mult` secam-direct
  predicate also caught MESECAM_VHS (SECAM colour system, block at the VHS
  colour-under carrier 732.4 kHz) and misrouted its chroma since the
  direct-composite commit; the block-centre criterion excludes it.

## What changed (tbc-tools, paired commit)

`SecamDecoder` output Y was a raw composite passthrough: fine for split-chroma
input, but a combined input carried the SECAM FM block into the video as luma
dots. Now, when the decoder's own `combinedInput` detection fires, the Y output
is the composite with a steep luma low-pass below the FM block floor (gain 1 to
3.1 MHz, raised cosine to 3.35 MHz, 0 above; anchored on the measured block
centre + carrier offset, rebuilt on recentre; per-row block-FFT like the chroma
masks). Split-chroma input keeps the untouched passthrough.

## Hard data

- Decoded output Y (etienne colourbars combined cut, 928x576 YUV444P16):
  block band 4.1-4.6 MHz = 0.00002 of luma (raw combined input: 0.264; old Y/C
  luma reference: 0.0068); block+sideband above 3.35 MHz = 0.0009. Steady
  carriers AND colour-transition sidebands gone (the transition sidebands were
  the visible "dot crawl" - a steady-block notch alone cannot catch them).
- cargo test --release: 25 passed. tbc-tools ctest: 33/33 passed (incl.
  decode-pretbc-secam-etienne-cbars + both MESECAM fixtures).
- Profile sweep (70 profiles): single-output True exactly for 819/PAL/SECAM
  QUADRUPLEX, NTSC/PAL/SECAM TYPEC, NTSC/PAL TYPEB, NTSC VHD; VHS/MESECAM_VHS/
  SECAM_VHS and all other colour-under profiles False.
- Full-tape decode (182 s SECAM quad RF, 30 threads): single 6.0 GB combined
  .tbc + sidecar, byte-exact field geometry, exit 0.
- decode.py --selftest SELFTEST OK; 30-check offscreen GUI state test OK.

## Physics note

The luma inside/around the FM block (3.9-4.76 MHz + edge sidebands) is
spectrally identical to chroma - no filter separates them from one signal.
Combined CVBS output therefore always carries the block in the raw .tbc (that
is the colour), and the decoded luma is clean only to the depth of the luma
low-pass (matched here to the user-validated separated-mode 3.2 MHz/order 12).
Zero dots + single file is not possible; zero dots + full colour = the old Y/C
two-file mode.

## Restore points

- tape-decode-rust: `RESTOREPOINT_CVBS_SINGLE_TBC_20261018.zip` (repo root)
- tbc-tools: `RESTOREPOINT_CVBS_LUMA_NOTCH_20261018.zip` (repo root)
