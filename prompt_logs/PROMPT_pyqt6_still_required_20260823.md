# PROMPT — "PyQt6 still required" report (20260823)

Log of inputs, commands, and findings for the user report that the
self-contained bundling has NOT worked and "PyQt6 is required" still occurs.

## User input
- "Well the python bundling has not worked, PyQT6 is required error still
  occurs.... it should be 100% self contained.."
- Also ran (pre-attached): `cd '..'` then `cd 'tape-decode-rust-gui'`
  (both finished, exit unknown).

## Commands run (this session)
- `ls C:\Users\Harry\tape-decode-rust-gui` -> repo root; `dist\decode-rust-gui.exe`
  present, plus `decode-rust-gui.spec`, `decode.py`, `decode_selftest.py`,
  `decode_launcher.py`, `requirements-launcher.txt`, prior PROMPT_*.md logs.
- `git --no-pager log --oneline -10` -> HEAD=master 042635e; fix commit
  70cc7fa "fix: make Windows launcher bundle self-contained (Qt6 + platform
  plugin)" present; tag v3.0.0 at 8c6e726 (pre-fix).
- `git --no-pager status` -> clean tree, up to date with origin/master.
- Read: `PROMPT_selfcontained_builds_20260823.md`, `decode-rust-gui.spec`,
  `requirements-launcher.txt`, `PROMPT_release_notes_20260823.md`,
  `decode.py`, `decode_selftest.py`, `scripts/ci/build-windows-decode-bin.py`,
  `decode_launcher.py` (1-199).
- grep `PyQt6.*required|pip install PyQt|...` -> the user-facing message
  "PyQt6 is required for Decode Launcher." originates at
  `decode_launcher.py:67` (the `except ImportError` around the PyQt6 imports).
- `QT_QPA_PLATFORM=offscreen; .\dist\decode-rust-gui.exe --selftest` ->
  **`SELFTEST OK`, SELFTEST_EXIT=0**. (Hard data: the current dist EXE's
  bundle imports PyQt6, loads a Qt platform plugin offscreen, and runs the
  bundled `tape-decode.exe` list-profiles.)
- `git log -1 --format=%cI 70cc7fa` -> fix committed 2026-08-23T12:59:55+01:00.
- `(Get-Item .\dist\decode-rust-gui.exe).LastWriteTime` ->
  2026-08-23T13:53:51+01:00 (built AFTER the fix). Length 90,347,175 bytes.

## Key findings (hard data)
- The local `dist\decode-rust-gui.exe` (built 13:53, after the 12:59 fix)
  PASSES the bundled `--selftest`: PyQt6 imports and Qt loads offscreen with
  no host Python/PyQt6. It does NOT reproduce "PyQt6 is required".
- "PyQt6 is required for Decode Launcher." (`decode_launcher.py:67`) only
  fires when `from PyQt6.QtCore/QtGui/QtWidgets import ...` raises
  `ImportError`. The selftest proves those modules import inside this bundle,
  so this EXE cannot emit that error.
- No new release tag has been pushed since v3.0.0 (pre-fix). The published
  v3.0.0 GitHub release is the OLD, non-self-contained build. Hypothesis: the
  user is launching the downloaded v3.0.0 release (or another old build), not
  this local dist EXE.
- CONTRADICTION: hard data (selftest OK on dist EXE) vs user report (still
  errors). Per project rules, NOT assuming — asking the user which build they
  are launching and the exact error text/appearance.

## Pending (user clarification requested)
- Which executable/path is being launched when the error appears.
- Exact error text and how it appears (console text / dialog / flash+close).
- Whether a fresh rebuild was attempted (and its result) vs a downloaded
  release.

## Next (after clarification)
- If user is on the v3.0.0 release: the fix is on master but unreleased;
  push a new `v*` tag to publish a self-contained release (release job gates
  on the strict `--selftest`).
- If user is on a freshly-rebuilt EXE that fails: capture the real build log
  + the failing import error, inspect the onefile payload
  (`pyinstaller --onefile` archive_viewer / `pyi-archive_viewer`) for
  Qt6Core/Qt6Gui/Qt6Widgets/qwindows.dll, and re-run the build with the
  current `scripts/ci/build-windows-decode-bin.py`.
- Do NOT assume the GUI works from the offscreen selftest alone; user must
  launch the actual GUI and confirm the window appears (pending).

## v4.0.0 verification (user: "I released v4.0.0 with the fixed code")
- User clarified: the failing launch was the downloaded v3.0.0 release
  (pre-fix, tagged 8c6e726 BEFORE fix commit 70cc7fa). Console text:
  "PyQt6 is required for Decode Launcher." v3.0.0 is the OLD non-self-contained
  build; the fix is on master and now released as v4.0.0.
- `git fetch --tags`; v4.0.0 -> 20cf8e4. `git merge-base --is-ancestor`:
  70cc7fa (Win Qt6/platform fix) AND af881ee (Linux GL fix) are BOTH ancestors
  of v4.0.0 (exit 0 each). Tagged code = fixed code.
- `gh release view v4.0.0`: published 2026-08-23T20:23:36Z, not draft/prerelease.
  Assets incl. decode-rust-gui-windows_4.0.0_x86_64.zip (106,975,570 B).
  build.yml run 32663464295 (master, workflow_dispatch) = completed/success.
- Downloaded v4.0.0 win x86_64 asset via `gh release download` -> extracted:
  single EXE decode-rust-gui-windows_4.0.0_x86_64.exe (108,033,477 B).
- STRUCTURAL PROOF (host-independent): `pyi-archive_viewer -l -b` on the v4.0.0
  EXE -> 3627 TOC entries. Verbatim critical entries:
    python312.dll  (embedded interpreter -> no host Python used)
    PyQt6\Qt6\bin\Qt6Core.dll / Qt6Gui.dll / Qt6Widgets.dll / Qt6Network.dll / Qt6Svg.dll
    PyQt6\QtCore.pyd / QtGui.pyd / QtWidgets.pyd  (+ .pyi)
    PyQt6\sip.cp312-win_amd64.pyd
    PyQt6\Qt6\plugins\platforms\qwindows.dll / qoffscreen.dll / qminimal.dll
      (platform plugins -> the exact thing whose absence -> "PyQt6 missing")
    tape-decode.exe (+ target-x86-64-v1..v4 microarch variants), profiles.json
  A PyInstaller onefile loads these from _MEIPASS; host site-packages is NOT
  on the frozen app's sys.path, and pip-installed host Qt6 DLLs are NOT on
  system PATH. => self-contained BY CONSTRUCTION regardless of host state.
- SCRUBBED-PATH RUNTIME TEST: removed every PATH entry matching
  Python|python|pip|Scripts|.cargo|rustup|conda|msys64|Qt; unset PYTHONHOME/
  PYTHONPATH/QT_QPA_PLATFORM_PLUGIN_PATH/QT_PLUGIN_PATH; `Get-Command python`
  and `py` both return nothing (PYTHON_ON_CLEAN_PATH=0). Ran the v4.0.0 EXE
  --selftest in that scrubbed env -> **SELFTEST OK, exit 0**. With no host
  Python discoverable, PyQt6 still imports + Qt loads offscreen => bundle.
- TRUE CLEAN-ROOM TEST: Windows Sandbox NOT available here
  (C:\Windows\WindowsSandbox.exe absent, HypervisorPresent=False,
  Get-WindowsOptionalFeature needs elevation). OS = Win10 Pro (Sandbox is
  supported but NOT enabled). Enabling needs admin + reboot -> NOT done;
  requires user approval. Alternatives: fresh VM / another PC with no Python.
- STILL PENDING (not proven by offscreen selftest): the real GUI window
  rendering with the `windows` platform plugin (qwindows.dll IS bundled, but
  selftest uses `offscreen`). User must launch the actual v4.0.0 GUI and
  confirm the window appears with no Python/PyQt6 errors.

## CLEAN-ROOM SANDBOX TEST (2026-08-26) — user: "Execute the sandbox test"
- Note: v4.0.0 GitHub RELEASE was deleted by user ("v4.0.0 was killed");
  gh release view v4.0.0 -> "release not found". Only v3.0.0/v2.0.0/v1.0.0
  remain published. BUT the actual published v4.0.0 Windows x86_64 binary was
  preserved from the earlier `gh release download` at
  %TEMP%\tdrg_v4_verify\extracted\decode-rust-gui-windows_4.0.0_x86_64.exe
  (108,033,477 B, 2026-08-23 20:22). Used that as the test binary (real
  release artifact, no network needed in the Sandbox).
- Staged on host: C:\Users\Harry\Desktop\tdrg_sandbox_test\ (EXE +
  run_test.ps1) and C:\Users\Harry\Desktop\tdrg_sandbox.wsb (Sandbox config,
  writable mapped folder -> C:\Sandbox\test, LogonCommand auto-runs the
  script). Sandbox feature enabled earlier via Enable-WindowsOptionalFeature
  Containers-DisposableClientVM (user ran admin commands + rebooted).
- Automated execution: closed any existing sandbox (one-instance limit),
  Start-Process tdrg_sandbox.wsb, polled host folder for result.txt. Result
  written back via the writable mapped folder at 2026-08-26T02:01:29Z
  (~30s after launch).
- VERBATIM RESULT (from the pristine Sandbox, NO Python/Qt on the image):
    python -> NOT FOUND (good)
    py     -> NOT FOUND (good)
    --selftest -> SELFTEST OK, SELFTEST_EXIT=0
    Real GUI smoke -> GUI process still alive after 8s (event loop running)
- User visual confirmation (2026-08-26): "Yes it opens fine" -> the Decode
  Launcher window renders in the no-Python Sandbox via the bundled
  qwindows.dll platform plugin.
- CONCLUSION (hard data, not assumption): the v4.0.0 Windows x86_64 onefile
  is 100% self-contained. On a pristine Windows image with NO Python and NO Qt
  installed, PyQt6 imports from the bundle, the Qt platform plugin loads from
  the bundle, the bundled tape-decode.exe runs, and the real GUI window opens.
  The reported "PyQt6 is required" was exclusively the pre-fix v3.0.0 release.
- RESTORE POINT: see RESTOREPOINT_V4_SELFCONTAINED_20260826.md + preserved
  zip (verified EXE + test artifacts + this log).
