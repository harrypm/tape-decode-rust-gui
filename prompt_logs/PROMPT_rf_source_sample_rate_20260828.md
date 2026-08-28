# PROMPT — rfSourceSampleRate in decode JSON sidecar (20260828)

Log of inputs, commands, and findings for the request to record the input RF
sample rate (the decoding frequency) into the `.tbc.json` sidecar as
`rfSourceSampleRate`, for easy post audio alignment.

## User input
- "I want to add set input sample rate i.g decoding freq sets json value
  rfSourceSampleRate for easy post audio alignment"

## Key findings (hard data from source)
- The input RF sample rate is already user-settable: CLI `--frequency`
  (`crates/tape-decode-cli/src/cli.rs:132-134`, `Option<f64>`, MHz, default
  40.0) -> `request.inputfreq` (`crates/tape-decode/src/request.rs:272`).
  The GUI already has a "Frequency (MHz)" `QLineEdit` (default "40") that
  emits `--frequency <value>` (`decode_launcher.py:453,540-541,718-720`).
  So no new user-interactable input was needed; the value already reaches the
  backend. The gap was that it was never written to the JSON.
- `request.inputfreq` is consumed only for filter design in
  `DecoderSpec::new` (`crates/tape-decode/src/spec.rs:163`). It is NOT in
  `DecoderMetadata`.
- `DecoderMetadata.sample_rate` (`crates/tape-decode/src/decode/mod.rs:642`,
  built at `:1626`) is the OUTPUT rate: `spec.sys_outfreq * 1_000_000.0`
  = 4fsc in Hz (e.g. PAL 17,734,475). This is what `videoParameters.sampleRate`
  already records. It is NOT the RF input rate.
- JSON sidecar is written by `crates/tape-decode-cli/src/writer.rs`
  (`append_tail` -> `metadata_to_tbc` -> `serde_json::to_vec`), spliced after
  the `fields` array. `metadata_to_tbc` (`metadata.rs:123`) builds
  `TbcMetadata { pcmAudioParameters, videoParameters }` from
  `DecoderMetadata` + `MetadataContext`.
- `MetadataContext` (`metadata.rs:67`) already carries request-level
  provenance (medium/format/system/git) built in `run_decode`
  (`cli.rs:487` from the profile name). This is the natural place to also
  carry the RF source sample rate (constant per decode, not per-field).
- `compare` (`cli.rs:883 compare_video_parameters`) already skips
  osInfo/gitBranch/gitCommit/system/tapeFormat as provenance, so adding
  another provenance field there is consistent and won't break baselines.

## Design decision
- Place: `videoParameters.rfSourceSampleRate`, next to `sampleRate`
  (both sample rates of the decode pipeline grouped together; avoids a lone
  root scalar).
- Unit: Hz (matches `videoParameters.sampleRate` which is in Hz), computed as
  `inputfreq_mhz * 1_000_000.0` (e.g. 40 MHz -> 40,000,000.0).
- `#[serde(default)]` so sidecars produced before this field still deserialize
  in `compare`; `compare` does not diff it (provenance, like system/git).

## Changes implemented
- `crates/tape-decode-cli/src/metadata.rs`:
  - `MetadataContext`: added `rf_source_sample_rate_hz: f64` (+ updated doc).
  - `MetadataContext::from_profile_name(name, rf_source_sample_rate_hz)`:
    new required param, stored into the context.
  - `VideoParameters`: added `#[serde(default)] rf_source_sample_rate: f64`
    (serialized `rfSourceSampleRate`, placed right after `sample_rate`).
  - `metadata_to_tbc`: sets `rf_source_sample_rate: ctx.rf_source_sample_rate_hz`.
  - Updated the 3 existing tests' `from_profile_name` calls to pass
    `40.0 * 1000_000.0`.
  - New test `metadata_to_tbc_writes_rf_source_sample_rate_in_hz`: builds a
    context with 40 MHz, serializes, parses the actual JSON bytes via
    `serde_json::Value`, and asserts
    `videoParameters.rfSourceSampleRate == 40_000_000.0`.
- `crates/tape-decode-cli/src/cli.rs`:
  - `run_decode`: computes
    `rf_source_sample_rate_hz = cli.frequency.unwrap_or(40.0) * 1_000_000.0`
    (same default the decoder uses for `request.inputfreq`) and passes it to
    `MetadataContext::from_profile_name`.
  - `compare_video_parameters`: added `// not checked: rfSourceSampleRate`
    comment (provenance).

## Commands run
- `cargo build --release --bin tape-decode` -> BUILD_EXIT=0 (1m 24s).
- `cargo test --release -p tape-decode-cli` -> TEST_EXIT=0; 5 passed, 0 failed
  (incl. new `metadata_to_tbc_writes_rf_source_sample_rate_in_hz`).
- End-to-end decode (real RF sample, 40 MSPS PAL SVHS, 16 frames):
  `./target/release/tape-decode decode --profile PAL_SVHS --frequency 40
   --input-format flac --luma-out /tmp/rfsr_test/out.tbc
   --metadata-out /tmp/rfsr_test/out.tbc.json --overwrite
   <decode-test-data>/decode-orc-testdata-vhs/flac/pal/yc/
   SVHS_PAL_EBU_Bars_MISRC_V2.5_40msps_12_bit_16_frames.flac`
  -> 33 fields decoded in 11.97s. Resulting `videoParameters` block:
  `"sampleRate": 17734475.0, "rfSourceSampleRate": 40000000.0, ...`.
- Default-frequency path (omit `--frequency`): same sample, `--profile PAL_SVHS`
  with no `--frequency` -> `rfSourceSampleRate = 40000000.0` (confirms
  `unwrap_or(40.0)` default records correctly).
- Note: first attempt used `--profile PAL_VHS` on the SVHS sample and produced
  0 fields ("Unable to find any sync pulses"); the SVHS capture needs the
  `PAL_SVHS` profile (wider filter band). Not a regression — just the wrong
  profile for that sample.

## Verification
- [x] `cargo build --release --bin tape-decode` succeeds.
- [x] `cargo test --release -p tape-decode-cli` -> 5 passed, 0 failed,
      including the new serialization test that parses the actual JSON bytes.
- [x] End-to-end: real RF decode writes `videoParameters.rfSourceSampleRate`
      = 40000000.0 to a real `.tbc.json` on disk (parsed with python json).
      Confirmed for both `--frequency 40` and the omitted-default path.
- [x] `compare` still compiles and skips the new field (provenance), so
      existing compare baselines that lack `rfSourceSampleRate` are unaffected.

## Notes / out of scope
- GUI (`decode_launcher.py`) unchanged: its existing "Frequency (MHz)" field
  already drives `--frequency`, so the value is now recorded automatically.
  No user-interactable element was changed, so no GUI real-world confirmation
  was required for this change.
- Not committed (user has not requested a commit for this feature).
- No restore-point zip created (user has not stated the change is
  fixed/fully working yet).
