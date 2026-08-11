# Prompt log: add `--subdeemp` checkbox (enabled by default) to Decode Launcher

Date: 2026-08-11

## User input / task
- "I want to add a box and enable by defult `--subdeemp`"
- Context: working in `/home/harry/tape-decode-rust` (Rust tape-decode CLI + PyQt6 GUI launcher).
- A `git pull` had just been run before the prompt (fast-forward `ac091cc..63c5265`).

## Commands run (by user, pre-prompt)
- `cd '..'`
- `cd 'tape-decode-rust'`
- `git pull` -> fast-forward, pulled in SECAM decode work + profile/spec changes.

## Commands run (by assistant)
- `read_executed_shell_command_output` on the `git pull` command id -> read pull output.
- `find /home/harry/tape-decode-rust -type f -name "*.rs" -not -path "*/target/*"` -> listed 28 Rust source files.
- `grep` for `subdeemp|sub_deemp|subDeemp` across the repo -> matched cli.rs, request.rs, spec.rs, demodblock.rs, profiles.json, format_params.json.
- `grep` for GUI/checkbox frameworks -> found `decode_launcher.py` (PyQt6) and `decode_runtime.py`.
- `grep` for `ire0_adjust_check|ire0-adjust|build_tape_decode_command` -> confirmed the existing `--ire0-adjust` checkbox is the pattern to mirror, and that `build_tape_decode_command` only prepends the binary path + passes args through.
- `python3 -m py_compile /home/harry/tape-decode-rust/decode_launcher.py` -> `SYNTAX_OK`.

## Files inspected (read)
- `/home/harry/tape-decode-rust/crates/tape-decode-cli/src/cli.rs` (lines 101-494)
  - `--subdeemp` is a plain boolean flag: `#[arg(long)] subdeemp: bool,` (cli.rs:199-200)
  - mapped into `DecodeRequest { subdeemp: cli.subdeemp, ... }` (cli.rs:394)
- `/home/harry/tape-decode-rust/crates/tape-decode/src/request.rs` (lines 171-315)
  - `pub subdeemp: bool,` field on `DecodeRequest` (request.rs:290)
- `/home/harry/tape-decode-rust/crates/tape-decode/src/spec.rs` (relevant ranges)
  - `video_subdeemp_enabled = decoder_params.nonlinear.use_sub_deemphasis || request.subdeemp;` (spec.rs:255-256)
  - stored on `DecoderSpec` as `video_subdeemp_enabled` (spec.rs:133, 718)
  - drives nonlinear bandpass build (spec.rs:420)
- `/home/harry/tape-decode-rust/decode_launcher.py` (full file)
  - PyQt6 GUI; existing checkboxes: `include_chroma_check`, `include_metadata_check`, `ire0_adjust_check` (default on), `overwrite_check`, `debug_check` (default off).
- `/home/harry/tape-decode-rust/decode_runtime.py` (lines 166-358)
  - `build_tape_decode_command` = `resolve_tape_decode_prefix(level) + list(args)` (decode_runtime.py:242-247). No special flag handling; appending `--subdeemp` is correct.

## Edits made
File: `/home/harry/tape-decode-rust/decode_launcher.py` (5 edits, all mirroring the `--ire0-adjust` checkbox)

1. Create the checkbox, checked by default (decode_launcher.py:450-451):
   ```python
   self.subdeemp_check = QCheckBox("Enable sub-deemphasis (--subdeemp)")
   self.subdeemp_check.setChecked(True)
   ```
2. Place it in the grid layout next to the debug checkbox (decode_launcher.py:520):
   ```python
   launch_layout.addWidget(self.subdeemp_check, 12, 2, 1, 2)
   ```
3. Wire its toggled signal to `_refresh_tool_state` (decode_launcher.py:556):
   ```python
   self.subdeemp_check.toggled.connect(self._refresh_tool_state)
   ```
4. Add it to the enable/disable widget tuple in `_refresh_tool_state` so it is disabled for non-decode tools (decode_launcher.py:617):
   ```python
   self.subdeemp_check,
   ```
5. Append `--subdeemp` to the built command when checked (decode_launcher.py:698-699):
   ```python
   if self.subdeemp_check.isChecked():
       args.append("--subdeemp")
   ```

## Validation done so far
- `python3 -m py_compile` -> SYNTAX_OK (no syntax errors).
- NOT yet validated in the running GUI (PyQt6 window not launched here).

## Pending real-world confirmation (per user rule on GUI changes)
- Launch `decode_launcher.py` and confirm:
  - The "Enable sub-deemphasis (--subdeemp)" checkbox is visible (row 12, right column, next to debug).
  - It is checked by default on startup.
  - The Terminal preview shows `--subdeemp` in the command when checked, and omits it when unchecked.
  - Toggling it updates the preview live.
  - It is disabled when a non-decode tool (list-profiles / compare / write-profile) is selected.
