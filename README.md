# tape-decode

A FM RF --> 4fsc .tbc (CVBS/S-Video) decoder for analog videotape tape formats, written in Rust. Ported from the [vhs-decode](https://github.com/oyvindln/vhs-decode) project, with a cut off at commit [fe3f6099](https://github.com/oyvindln/vhs-decode/commit/fe3f6099e9e6a77295f26585598f658f2d926bb4).

Workflow Example

VCR --> ADC --> MISRC GUI --> FLAC FM RF Files --> Tape Decode --> TBC-Tools --> FFV1 Video Files


## GUI


<img width="500" height="" alt="launcher" src="https://github.com/user-attachments/assets/3c0cc10b-f141-4248-a457-5b0fc1833030" />

> Tape Decode GUI Launcher

<img width="800" height="" alt="decoded" src="https://github.com/user-attachments/assets/7f1ac4d0-7f60-405a-871b-13ae82de4981" />

> Decoded Result in [tbc-tools](https://github.com/harrypm/tbc-tools) tbc-analyse. 


## Installation


You can install for Windows / MacOS / Linux for x86 and ARM64 via self-contained binary [releases here](https://github.com/harrypm/tape-decode-rust-gui/releases)

For x86-64, ensure you use the correct one for your [CPU feature level](https://en.wikipedia.org/wiki/X86-64#Microarchitecture_levels).

Use nightly Rust for new feature testing builds.


### From source


```bash
RUSTFLAGS="-C target-cpu=native" cargo build --release
```

## Usage

```bash
tape-decode --help
```

<details>
<summary><code>tape-decode</code> CLI command reference</summary>

Captured from the built binary (click to expand). Run `tape-decode <command> --help` for the same output.

Top-level (`tape-decode --help`):

```text
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

`decode` (`tape-decode decode --help`):

```text
Decode an RF capture

Usage: tape-decode decode [OPTIONS] --luma-out <LUMA_OUT> <--profile <PROFILE>|--profile-file <PROFILE_FILE>> <INFILE>

Arguments:
  <INFILE>
          Input RF capture file, or `-` to read from standard input

Options:
      --profile <PROFILE>
          Profile name

      --profile-file <PROFILE_FILE>
          Path to a profile to load instead of an embedded profile

      --frequency <FREQUENCY>
          Input RF sample rate in MHz; Hz, kHz, MHz, M, and k suffixes are accepted

      --offset <OFFSET>
          Input sample offset to seek before decoding

      --input-format <INPUT_FORMAT>
          Input format

          Possible values:
          - u8:    Unsigned 8-bit samples
          - s8:    Signed 8-bit samples
          - s16le: Signed little-endian 16-bit samples
          - u16le: Unsigned 16-bit samples
          - f32le: Little-endian 32-bit float samples
          - flac:  Mono FLAC stream (decoded to its native bit depth)
          
          [default: u8]

      --overwrite
          Allow overwriting outputs

      --debug
          Enable debug-level logging unless RUST_LOG supplies an explicit filter

      --export-raw-tbc
          Export raw f32 TBC luma

      --luma-out <LUMA_OUT>
          Luma output path, or `-` to write to standard output

      --chroma-out <CHROMA_OUT>
          Chroma output path, or `-` to write to standard output

      --metadata-out <METADATA_OUT>
          Metadata output path

      --chroma-trap
          Apply a chroma trap to the luma path

      --sharpness <SHARPNESS>
          Video EQ sharpening amount; 0 disables sharpening
          
          [default: 0]

      --notch <NOTCH>
          Apply an RF notch filter at the given frequency in MHz

      --notch-q <NOTCH_Q>
          Q factor for the RF notch filter
          
          [default: 10]

      --ntscj
          Treat NTSC black level as 0 IRE instead of 7.5 IRE

      --level-adjust <LEVEL_ADJUST>
          MAD multiplier used when suppressing wow level-adjustment outliers
          
          [default: 0.1]

      --ire0-adjust
          Adjust RF IRE0 from measured picture content

      --high-boost <HIGH_BOOST>
          Override the profile's RF high-boost multiplier

      --disable-diff-demod
          Disable differential video demodulation

      --fm-audio-notch [<FM_AUDIO_NOTCH>]
          Enable FM audio notch filters; omitting the value uses Q=10.0

      --enable-dc-offset
          Apply detected RF DC-offset correction to the decoded video

      --nldeemp
          Enable nonlinear deemphasis

      --subdeemp
          Enable sub-deemphasis

      --y-comb [<Y_COMB>]
          Enable luma Y comb filtering with the given IRE clamp; omitting the value uses 1.5
          
          [default: 0.0]

      --cafc[=<CAFC>]
          Override chroma automatic frequency control; omitting the value enables it
          
          [possible values: true, false]

      --track-phase <TRACK_PHASE>
          Force a chroma track phase instead of using the detected/default phase

      --detect-chroma-track-phase
          Detect chroma track phase from the decoded signal

      --disable-phase-correction
          Disable chroma phase correction

      --disable-burst-hsync
          Disable burst-based hsync correction

      --no-comb
          Disable chroma comb filtering

      --disable-right-hsync
          Disable the right-edge hsync zero-crossing refinement

      --level-detect-divisor <LEVEL_DETECT_DIVISOR>
          Divisor for the level-detection pass sample rate
          
          [default: 3]

      --fallback-vsync[=<FALLBACK_VSYNC>]
          Override fallback vsync recovery; omitting the value enables it
          
          [possible values: true, false]

      --relaxed-line0
          Relax line-0 recovery checks when sync is difficult to lock

      --field-order-confidence <FIELD_ORDER_CONFIDENCE>
          Required confidence percentage for accepting detected field order
          
          [default: 100]

      --field-order-action <FIELD_ORDER_ACTION>
          Action to take when consecutive fields have the same detected order
          
          [possible values: detect, duplicate, drop, none]

      --use-saved-levels
          Reuse previously detected levels until sync issues require recalculation

      --skip-hsync-refine
          Skip hsync location refinement after initial pulse detection

      --no-dod
          Disable RF dropout detection

      --dod-threshold-p <DOD_THRESHOLD_P>
          RF dropout threshold as a fraction of the field average envelope
          
          [default: 0.18]

      --dod-threshold-a <DOD_THRESHOLD_A>
          Absolute RF dropout threshold, overriding the percentage threshold

      --dod-hysteresis <DOD_HYSTERESIS>
          Hysteresis ratio used by RF dropout detection
          
          [default: 1.25]

      --wow-level-adjust-smoothing <WOW_LEVEL_ADJUST_SMOOTHING>
          Smoothing window, in lines, for wow level adjustment

      --wow-interpolation-method <WOW_INTERPOLATION_METHOD>
          Interpolation method for wow correction
          
          [default: linear]
          [possible values: linear, quadratic, cubic]

      --mt-threads <MT_THREADS>
          Number of decoding threads; 0 decodes serially on a single thread
          
          [default: 0]

      --mt-distance-size <MT_DISTANCE_SIZE>
          Fields of distance between each thread's start, and the overlap width searched for a stitch
          
          [default: 20]

      --mt-overlap-count <MT_OVERLAP_COUNT>
          Consecutive matching fields required to stitch one thread onto the next
          
          [default: 2]

      --mt-threshold <MT_THRESHOLD>
          Per-field MSRE threshold below which overlapping fields are treated as matching
          
          [default: 64]

      --mt-trim-fraction <MT_TRIM_FRACTION>
          Fraction of the largest per-sample deviations discarded before averaging when matching fields
          
          [default: 0.1]

  -h, --help
          Print help (see a summary with '-h')
```

`write-profile` (`tape-decode write-profile --help`):

```text
Flatten a named profile and write it to a JSON file

Usage: tape-decode write-profile [OPTIONS] <--profile <PROFILE>|--profile-file <PROFILE_FILE>> <OUT>

Arguments:
  <OUT>  Destination path for the flattened profile JSON

Options:
      --profile <PROFILE>            Profile key from the embedded profile table
      --profile-file <PROFILE_FILE>  Path to a JSON profile object to load instead of an embedded profile key
      --overwrite                    Allow replacing an existing output file
  -h, --help                         Print help
```

`list-profiles` (`tape-decode list-profiles --help`):

```text
List the names of all embedded profiles

Usage: tape-decode list-profiles

Options:
  -h, --help  Print help
```

`compare` (`tape-decode compare --help`):

```text
Compare two decode outputs

Usage: tape-decode compare [OPTIONS]

Options:
      --metadata <REFERENCE> <CANDIDATE>
          Reference and candidate metadata sidecars (`.tbc.json`)
      --luma <REFERENCE> <CANDIDATE>
          Reference and candidate luma `.tbc` files
      --chroma <REFERENCE> <CANDIDATE>
          Reference and candidate chroma `_chroma.tbc` files
      --threshold <THRESHOLD>
          Per-field MSRE threshold below which TBC fields are considered matching [default: 64]
      --trim-fraction <TRIM_FRACTION>
          Fraction of the largest per-sample squared deviations discarded before averaging [default: 0.1]
      --float-abs-tol <FLOAT_ABS_TOL>
          Absolute tolerance for comparing JSON float values [default: 0.11]
      --float-rel-tol <FLOAT_REL_TOL>
          Relative tolerance for comparing JSON float values [default: 0.000000001]
  -h, --help
          Print help
```

</details>

## Decode Launcher GUI (decode-rust-gui)

The repository includes a Qt6 launcher (`decode.py` + `decode_launcher.py`) modeled after the vhs-decode Decode Launcher and wired to `tape-decode`.

### Run from source

```bash
python3 -m venv .venv-launcher
source .venv-launcher/bin/activate
python -m pip install -r requirements-launcher.txt
python decode.py
```

If your distro uses an externally-managed Python environment (PEP 668), use this venv flow instead of installing launcher dependencies into system Python.

The launcher defaults to guided `tape-decode decode` command creation (profile/output/frequency/threads), and can also run `list-profiles`, `compare`, and `write-profile` in a terminal.

### CLI passthrough via dispatcher

`decode.py` also forwards normal CLI args directly to `tape-decode`:

```bash
python3 decode.py decode --profile PAL_VHS --luma-out out.tbc capture.flac
```

### Build/package notes

- Windows EXE launcher bundle: `scripts/ci/build-windows-decode-bin.py`
- macOS app bundle launcher: `scripts/ci/build-macos-decode-bin.py`
- Linux launcher binary for AppImage staging: `scripts/ci/build-linux-decode-bin.py`
- Shared Git version resolver (MISRC-style): `scripts/ci/git-version.sh`

Linux local packaging sequence (matching CI workflow):

```bash
cargo build --release --target x86_64-unknown-linux-gnu --bin tape-decode
source .venv-launcher/bin/activate
python -m pip install pyinstaller -r requirements-launcher.txt
TAPE_DECODE_BIN=target/x86_64-unknown-linux-gnu/release/tape-decode \
  python scripts/ci/build-linux-decode-bin.py
```

For Linux arm64 local builds, replace `x86_64-unknown-linux-gnu` with `aarch64-unknown-linux-gnu` in both commands.

GitHub Actions release formatting now mirrors MISRC:
- `workflow_dispatch` supports `create_release` and `release_tag` inputs.
- Artifact names are versioned and architecture-scoped:
  - `decode-rust-gui-linux_<version>_<arch>.zip` / `.AppImage`
  - `decode-rust-gui-windows_<version>_<arch>.exe` / `.zip`
  - `decode-rust-gui-macos_<version>_<arch>.dmg` / `.zip`
- Version is resolved from tags (`v*`) or `scripts/ci/git-version.sh` fallback (`dev-<sha>` style).

For release artifacts, trigger:
- `.github/workflows/build_windows_decode.yml`
- `.github/workflows/build_macos_decode.yml`
- `.github/workflows/build_linux_decode.yml`

### Examples

**List available profiles**

```bash
tape-decode list-profiles
```

Output:

```text
405_BETAMAX
819_QUADRUPLEX
MESECAM_VHS
...
```

**Decode a 40 MHz PAL VHS tape from `capture.flac`**

```bash
tape-decode decode \
  --luma-out decoded.tbc \
  --chroma-out decoded_chroma.tbc \
  --metadata-out decoded.tbc.json \
  --profile PAL_VHS \
  --frequency 40 \
  --input-format flac \
  capture.flac
```

**Decode a 16 MHZ NTSC VHS tape from `capture.u8`, with 16 threads and 60 field per-thread offset**

```bash
tape-decode decode \
  --luma-out decoded.tbc \
  --chroma-out decoded_chroma.tbc \
  --metadata-out decoded.tbc.json \
  --profile NTSC_VHS \
  --frequency 16 \
  --mt-threads 16 \
  --mt-distance-size 60 \
  capture.u8
```

**Livestream 40 MHz PAL VHS from `/dev/cxadc0`**

```bash
cat /dev/cxadc0 \
  | tape-decode decode \
    --luma-out - \
    --profile PAL_VHS \
    --frequency 40 \
    --mt-threads 16 \
    --mt-distance-size 60 \
    - \
  | ffmpeg \
    -f rawvideo \
    -pixel_format gray16le \
    -video_size 1135x626 \
    -r 25 \
    -i - \
    -f yuv4mpegpipe \
    -filter:v "format=yuv444p" \
    - \
  | mpv -
```

## Using in your project

The tape-decode crate hosting the main decoder can be used as a library in your Rust project. You can also use a `cdylib` to call the decoder from other languages.

## License

This project is based on vhs-decode, which is licensed under GPL-3.0. The Rust port is also licensed under GPL-3.0. See [COPYING](COPYING) for details.
