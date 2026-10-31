# tape-decode

A decoder for analog tape formats, written in Rust. Ported from the [vhs-decode](https://github.com/oyvindln/vhs-decode) project, commit [a33a5bf2](https://github.com/oyvindln/vhs-decode/commit/a33a5bf2d6ade95592e522cccf6095f9ef4da8d1).

## Installation

### From source

Use nightly Rust for best performance builds.

```bash
RUSTFLAGS="-C target-cpu=native" cargo build --release
```

### Pre-built binaries

Pre-built binaries for x86-64 and aarch64 Windows and Linux (glibc) are available in Releases. For x86-64, ensure you use the correct one for your [CPU feature level](https://en.wikipedia.org/wiki/X86-64#Microarchitecture_levels).

## Usage

```bash
tape-decode --help
```

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
