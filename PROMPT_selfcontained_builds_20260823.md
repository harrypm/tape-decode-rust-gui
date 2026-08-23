# PROMPT — self-contained builds (20260823)

Log of inputs, commands, and findings for the request to make
`tape-decode-rust-gui` builds 100% self-contained (users reported
"python being required / pyqt6 missing").

## User input
- "clone https://github.com/harrypm/tape-decode-rust-gui and I want to make
  sure all the builds are 100% self contained as some users have reported
  python being required ptyqt6 missing..."
- Initial `clone ...` was typed literally into pwsh and failed (`clone` is not
  a cmdlet). Correct invocation is `git clone`.
- Scope decisions (approved): Required only (Windows bundling parity +
  `--selftest` mode + strict CI checks on all 3 OSes). Do NOT pin PyQt6
  versions (leave `PyQt6>=6.6,<7`). User will launch
  `dist\decode-rust-gui.exe` themselves to verify the GUI.

## Commands run
- `git clone https://github.com/harrypm/tape-decode-rust-gui` -> ok (735 objects, 2.02 MiB)
- `ls tape-decode-rust-gui` -> repo top level (Rust workspace + Python launcher)
- `ls -R crates; scripts; .github; .cargo` -> layout map
- `grep` for `from PyQt6` / `import PyQt6` -> launcher imports at decode_launcher.py:47-49
- `file_glob *.spec` / `hooks-PyQt6*` / `rthooks/**` -> NONE (no spec/hooks override)
- web search PyQt6-Qt6 wheels -> win_amd64 + win_arm64 wheels exist (6.11.1); Qt6 supports Windows-on-ARM

## Key findings (hard data)
- Architecture: Rust core (`crates/tape-decode`, `crates/tape-decode-cli` ->
  `tape-decode` binary) + PyQt6 launcher (`decode.py`/`decode_launcher.py`/
  `decode_runtime.py`) packaged via PyInstaller `--onefile`.
- Root cause: `scripts/ci/build-windows-decode-bin.py` passes ONLY
  `--collect-all PyQt6`. macOS (`build-macos-decode-bin.py:81-82,110-118`) and
  Linux (`build-linux-decode-bin.py:81-82,107-115`) ALSO pass
  `--collect-all PyQt6-Qt6` and explicitly `--add-data PyQt6/Qt6/plugins`.
  Without those, the Windows onefile EXE is missing Qt6 runtime DLLs +
  `qwindows.dll` -> "no Qt platform plugin could be initialized" -> users read
  as "PyQt6 missing / Python required".
- CI gap: `build_windows_decode.yml:123-144` never runs the EXE (only checks
  ZIP is flat). `build_linux_decode.yml:173-177` runs `list-profiles` with
  `|| true` (broken builds pass). None exercise PyQt6.
- Plan: see `create_plan` (plan_id d78e87bc-3176-4061-99d9-dced77221999).

## Changes implemented
- `scripts/ci/build-windows-decode-bin.py`: added `--collect-all PyQt6-Qt6`,
  `--collect-all PyQt6-sip`, `--hidden-import decode_selftest`, the
  `PyQt6/Qt6/plugins` `--add-data` block (parity with mac/linux), and `-y --clean`.
- `decode.py`: added `SELFTEST_FLAGS` and a `--selftest`/`--check-gui-deps` branch
  (before CLI passthrough) that calls `decode_selftest.run_selftest()`.
- `decode_selftest.py` (new): offscreen QApplication + `decode_runtime.list_profiles`;
  prints `SELFTEST OK` / exit 0, else non-zero with a clear message.
- `scripts/ci/build-macos-decode-bin.py` + `build-linux-decode-bin.py`: added
  `--hidden-import decode_selftest`.
- `.github/workflows/build_windows_decode.yml`: added strict `--selftest` step
  (`QT_QPA_PLATFORM=offscreen`, grep `SELFTEST OK`) before packaging.
- `.github/workflows/build_macos_decode.yml`: added strict `--selftest` step
  after codesign, before DMG.
- `.github/workflows/build_linux_decode.yml`: added strict `--selftest` step
  after the onefile build; replaced the `|| true` list-profiles AppImage block
  with a strict `--selftest` on the AppImage.
- `py_compile` on all 5 edited Python files -> `PY_COMPILE_OK` (no syntax errors).

## Verification
- [x] edited Python files compile (`py_compile` -> `PY_COMPILE_OK`)
- [x] local full build DONE:
  - Rust binary: `C:\Users\Harry\.cargo\bin\cargo.exe build --release --bin
    tape-decode` -> `target\release\tape-decode.exe` (5,968,384 bytes, 2m06s).
    (Rust was installed at ~/.cargo but not on PATH; invoked by full path.)
  - Installed python.org Python 3.12.10 (amd64, per-user, with pip) — winget was
    unavailable so used the python.org installer directly.
  - `pip install pyinstaller PyQt6 PyQt6-Qt6 PyQt6-sip` -> PyQt6 6.11.0,
    PyQt6-Qt6 6.11.1, PyQt6-sip 13.12.0, pyinstaller 6.22.2.
  - `python scripts\ci\build-windows-decode-bin.py` -> BUILD_EXIT=0; log line
    `Adding Qt plugins from ...\PyQt6\Qt6\plugins` confirms the new plugins
    block ran. Produced `dist\decode-rust-gui.exe` (90,347,229 bytes, ~86 MB).
- [x] onefile payload contains Qt6Core.dll, Qt6Gui.dll, qwindows.dll
  (archive_viewer entries): `PyQt6\Qt6\bin\Qt6Core.dll`,
  `PyQt6\Qt6\bin\Qt6Gui.dll`, `PyQt6\Qt6\plugins\platforms\qwindows.dll`,
  `qoffscreen.dll`, `qminimal.dll`, plus `tape-decode.exe` and icon PNGs.
- [x] bundled EXE `--selftest` (QT_QPA_PLATFORM=offscreen, clean shell) ->
  `SELFTEST OK`, SELFTEST_EXIT=0. Proves Qt platform plugin loads from the
  bundle AND the bundled tape-decode.exe runs list-profiles — no host
  Python/PyQt6 needed. This is the runtime proof that the reported defect
  ("PyQt6 missing / Python required") is fixed.
- [ ] user launches dist\decode-rust-gui.exe and confirms the GUI window
      appears with no Python/PyQt6 errors (pending user — do NOT assume from
      the offscreen selftest alone).
- [ ] 3 workflows pass the new --selftest steps on x86_64 + arm64 (requires
      push + GitHub Actions run — not yet done).

## Toolchain findings (hard data)
- `cargo`/`rustc`: present at `C:\Users\Harry\.cargo\bin\cargo.exe` (+ ~/.rustup)
  but NOT on this shell's PATH. Used by full path.
- pre-existing `python`: C:\msys64\mingw64\bin\python.exe (MSYS2 mingw64) 3.14.5;
  pip NOT installed ("No module named pip") — unsuitable for PyPI PyQt6 wheels.
- installed `python`: C:\Users\Harry\AppData\Local\Programs\Python\Python312\
  python.exe (python.org 3.12.10, pip 25.0.1 -> upgraded 26.2.1).
- `winget` not available; fell back to the python.org installer.
- PyInstaller supports Python 3.14 (added in 6.15.0); 6.22.2 used on 3.12.
- PyQt6-Qt6 ships win_amd64 + win_arm64 wheels (confirmed via PyPI).
- `.gitignore` covers `__pycache__/`, `*.pyc`, `/build`, `/dist`, `/*.spec`
  (all build artifacts ignored; no stray tracked files added except the
  intended `decode_selftest.py` and this log).
