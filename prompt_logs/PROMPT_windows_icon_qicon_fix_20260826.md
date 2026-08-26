# PROMPT — Fix Windows icon CI failure (QIcon(exe) impossible condition) (20260826)

User request: "https://github.com/harrypm/tape-decode-rust-gui/actions/runs/32918326586/job/98026605174 - fix build issues"

## Failing CI
- Run #44 "Build and release binary" (`build.yml`), job `Build decode-rust-gui Windows (x86_64)` and `Windows (arm64)`.
- Only failing step: `Verify bundled Windows icon payload + embedded EXE icon`.
- Verbatim failure: `QIcon(exe) failed to load embedded Windows icon resource` -> `Process completed with exit code 1.` (both arches).
- Selftest step PASSED (`SELFTEST OK`). PyInstaller logged `Copying icon to EXE` (icon WAS embedded). Bundled-entries TOC check passed.
- So the icon was embedded and bundled; only the `QIcon(exe)` load check failed.

## Commands run + hard data (all on host, Python 3.12.10 at C:\Users\Harry\AppData\Local\Programs\Python\Python312\python.exe)

### 1) gh CLI log inspection
- `gh run view 32918326586 --repo harrypm/tape-decode-rust-gui --log-failed`
- `gh run view --job 98026605174 --repo harrypm/tape-decode-rust-gui --log`
- Confirmed failure is exactly `QIcon(exe) failed to load embedded Windows icon resource`.

### 2) Qt icon-behavior probe (decisive hard data)
QIcon(path) / QFileIconProvider on real executables + our .ico:
- `QIcon(C:\Windows\System32\cmd.exe)`      -> isNull=True sizes=[]
- `QIcon(C:\Windows\explorer.exe)`          -> isNull=True sizes=[]
- `QIcon(C:\Windows\System32\notepad.exe)`  -> isNull=True sizes=[]
- `QIcon(C:\Windows\regedit.exe)`           -> isNull=True sizes=[]
- `QIcon(...\resources\icon\tape-decode-rust.ico)` -> isNull=False sizes=[(16,16),(24,24),(32,32),(48,48),(64,64),(128,128),(256,256)]
- `QFileIconProvider().icon(QFileInfo(cmd.exe))`     -> isNull=False sizes=[(16,16),(32,32),(128,128)]
- `QFileIconProvider().icon(QFileInfo(explorer.exe))`-> isNull=False sizes=[(16,16),(32,32),(128,128)]
- QImageReader.supportedImageFormats() includes `ico` (qico plugin present).
- `pefile` available: 2024.8.26.

CONCLUSION: `QIcon(exe_path)` does NOT read PE-embedded icons (Qt's QIcon(path) loads image files, not PE resources). It is null for real exes that definitely have embedded icons. The prior local "passing" probe (PROMPT_windows_icon_header_embedding_20260826.md) used the `.ico` file path, NOT `QIcon(exe)`, so it never validated the CI condition. The runtime's `QIcon(sys.executable)` block was dead code (always null -> fell back to bundled .ico).

### 3) pefile PE-resource probe on real exes
- cmd.exe:     resource_type_ids=[3,14,16,24] has_GROUP=True has_ICON=True
- explorer.exe:resource_type_ids=[3,14,16,24,256] has_GROUP=True has_ICON=True
- (RT_ICON=3, RT_GROUP_ICON=14) -> snippet works.

### 4) Local full repro build (PyInstaller 6.22.2, prebuilt target/release/tape-decode.exe)
- `python scripts\ci\build-windows-decode-bin.py` -> BUILD_EXIT=0
- Produced `dist\decode-rust-gui.exe` (90,647,808 bytes). UPX warnings non-fatal.

### 5) New CI verification logic run against the real built EXE
- TOC_CHECK_OK
- EMBEDDED_ICON_OK [(16,16),(32,32),(128,128)]  (QFileIconProvider reads embedded PE icon)
- PE_RESOURCE_OK [3, 14, 24]                      (RT_ICON+RT_GROUP_ICON present, survived UPX)
- FALLBACK_ICO_OK [(16,16),(24,24),(32,32),(48,48),(64,64),(128,128),(256,256)]
- WINDOWS_ICON_OK ... VERIFY_EXIT=0

### 6) Bundled selftest after runtime change
- `dist\decode-rust-gui.exe --selftest` -> `SELFTEST OK`, SELFTEST_EXIT=0

## Root cause
CI `Verify ... embedded EXE icon` step tested `QIcon(dist/decode-rust-gui.exe)`, which is null for ANY Windows executable because Qt's `QIcon(path)` constructor reads image files via image-format plugins, not PE-embedded icon resources. The embedded icon was present (PyInstaller `--icon` worked; pefile confirms RT_GROUP_ICON/RT_ICON); the check was simply testing an impossible condition.

## Changes made
- `decode_launcher.py`
  - Added imports: `QFileInfo` (QtCore), `QFileIconProvider` (QtWidgets).
  - `main()`: replaced dead `QIcon(sys.executable)` (always null on Windows) with
    `QFileIconProvider().icon(QFileInfo(sys.executable))`, which actually reads the
    embedded PE icon; keeps the bundled `.ico` file fallback below it.
- `.github/workflows/build.yml` (windows-decode job)
  - Install step: `pip install pyinstaller pefile -r requirements-launcher.txt`.
  - `Verify bundled Windows icon payload + embedded EXE icon` step: replaced
    `QIcon(exe)` with (a) `QFileIconProvider().icon(QFileInfo(exe))` non-null+sizes,
    (b) hard `pefile` PE-resource check for RT_GROUP_ICON+RT_ICON, (c) bundled
    `.ico` fallback `QIcon` load. Kept the existing TOC payload check.
- `.github/workflows/build_windows_decode.yml`
  - Same install + verify-step changes as build.yml.
- `decode-rust-gui.spec` left untouched (not used by CI; CI uses build-windows-decode-bin.py).

## Verification status
- py_compile decode_launcher.py -> PY_COMPILE_EXIT=0
- Local repro build -> BUILD_EXIT=0
- New verify logic against real EXE -> VERIFY_EXIT=0 (all four checks pass)
- Bundled --selftest -> SELFTEST OK / exit 0
- NOT yet committed/pushed. NOT yet visually confirmed by user (titlebar/taskbar icon).

## Notes
- CI environment may or may not have UPX on PATH; locally UPX was applied and the
  PE icon resource survived (pefile still reports RT_GROUP_ICON). The new pefile
  check will catch any future regression where the icon fails to embed.
- `QFileIconProvider` returns the shell icon for the exe = the embedded PE icon
  (proven on cmd.exe/explorer.exe and on the real built launcher).
