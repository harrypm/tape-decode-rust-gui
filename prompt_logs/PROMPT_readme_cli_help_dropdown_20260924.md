# Prompt Log — README CLI help closed dropdown

- Date: 2026-09-24
- Repo: /home/harry/tape-decode-rust
- Branch: master

## User input (prompt)

> put tape decode command list into an cli help closed dropdown on the readme

Also noted: user ran `cd ..` then `cd tape-decode-rust` (shell navigation; outputs not read, not relevant).

## Goal

Capture the real `tape-decode` CLI command list from the built binary and add it to `README.md`
as a collapsed `<details>` dropdown (closed by default — no `open` attribute) under the
`## Usage` section.

## Commands run and their output

### 1. List project root

```
ls -la /home/harry/tape-decode-rust
```

Found: `README.md`, built binary `target/release/tape-decode`, `crates/tape-decode-cli/`,
`prompt_logs/`, `scripts/`.

### 2. Locate README and markdown files

```
find /home/harry/tape-decode-rust -maxdepth 2 -iname "readme*" -o -maxdepth 2 -iname "*.md"
```

Confirmed `README.md` at repo root and existing `prompt_logs/PROMPT_*.md` naming convention.

### 3. Read current README.md

`README.md` had a minimal `## Usage` section:

```
## Usage

```bash
tape-decode --help
```
```

Followed by `## Decode Launcher GUI (decode-rust-gui)`.

### 4. Find built binary + CLI source

```
find /home/harry/tape-decode-rust/target -maxdepth 4 -name "tape-decode*" -type f
find /home/harry/tape-decode-rust/crates/tape-decode-cli -maxdepth 3 -type f
```

Built binary present: `target/release/tape-decode`.
CLI source: `crates/tape-decode-cli/src/cli.rs`, `main.rs`, `decode.rs`, etc.

### 5. Capture top-level help (hard data)

```
/home/harry/tape-decode-rust/target/release/tape-decode --help 2>&1
```

Output (exit 0):

```
Extracts video from RAW RF captures of colour-under & composite modulated tapes

Usage: tape-decode <COMMAND>

Commands:
  decode         Decode an RF capture
  write-profile  Flatten a named profile and write it to a JSON file
  list-profiles  List the names of all embedded profiles
  compare        Compare two decode outputs
  help           Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

### 6. Capture each subcommand help (hard data)

```
for c in decode write-profile list-profiles compare; do
  ./target/release/tape-decode "$c" --help 2>&1
done
```

All exited 0. Outputs (verbatim) embedded in the README dropdown. Summary:

- `decode` — long-form help with ~50 options (profile/profile-file/frequency/offset/
  input-format/overwrite/debug/export-raw-tbc/luma-out/chroma-out/metadata-out/chroma-trap/
  sharpness/notch/notch-q/ntscj/level-adjust/ire0-adjust/high-boost/disable-diff-demod/
  fm-audio-notch/enable-dc-offset/nldeemp/subdeemp/y-comb/cafc/track-phase/
  detect-chroma-track-phase/disable-phase-correction/disable-burst-hsync/no-comb/
  disable-right-hsync/level-detect-divisor/fallback-vsync/relaxed-line0/
  field-order-confidence/field-order-action/use-saved-levels/skip-hsync-refine/no-dod/
  dod-threshold-p/dod-threshold-a/dod-hysteresis/wow-level-adjust-smoothing/
  wow-interpolation-method/mt-threads/mt-distance-size/mt-overlap-count/mt-threshold/
  mt-trim-fraction).
- `write-profile` — `<OUT>` + `--profile`/`--profile-file`/`--overwrite`.
- `list-profiles` — no options beyond `-h`.
- `compare` — `--metadata`/`--luma`/`--chroma` pairs + `--threshold`/`--trim-fraction`/
  `--float-abs-tol`/`--float-rel-tol`.

## Change made

Edited `README.md` `## Usage` section: kept the `tape-decode --help` hint, then appended a
`<details>` dropdown titled `tape-decode CLI command reference` (closed by default) containing
the top-level help plus all four subcommand help blocks in `text` code fences, captured
verbatim from the built binary.

No source code or binary changed — documentation only.

## Verification notes / pending confirmation

- The CLI text was copied verbatim from the built `target/release/tape-decode` binary's
  `--help` output (verified against hard data, not assumed).
- `<details>`/`<summary>` rendering and the collapsed-by-default behavior are rendered by
  GitHub — cannot be verified locally. User should confirm the dropdown collapses/expands
  as expected on the GitHub README view.
