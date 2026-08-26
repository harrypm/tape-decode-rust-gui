# PROMPT — Windows icon/header embedding fix + CI regression guard (20260826)

User request: "fix icon window headder embeedeing for windows", then
"Yup fixed update GH actions, and commit/push".

## Scope implemented
- Runtime launcher icon behavior on Windows (titlebar/taskbar identity + icon source).
- Windows packaging script icon payload (ICO files bundled for runtime fallback).
- GitHub Actions Windows jobs updated to verify icon payload + embedded EXE icon load.

## Hard findings before changes
- `decode_launcher.py` runtime icon resolution only searched PNG files.
- On Windows, reliable taskbar/titlebar icon behavior benefits from:
  1) explicit AppUserModelID, and
  2) using the icon resource embedded in the built EXE when frozen.
- Windows build script already used `--icon resources/icon/tape-decode-rust.ico`
  for EXE resource, but did not bundle `.ico` files as runtime data.

## Changes made
- `decode_launcher.py`
  - Added `WINDOWS_APP_USER_MODEL_ID` constant.
  - Added `_set_windows_app_user_model_id()` using
    `SetCurrentProcessExplicitAppUserModelID(...)`.
  - Called `_set_windows_app_user_model_id()` before `QApplication(...)`.
  - Extended `_resolve_icon_path()` to search `.ico` paths on Windows
    (in `_MEIPASS`, next to executable, and source-tree locations), before PNG fallbacks.
  - In `main(...)`, on frozen Windows builds now prefer `QIcon(sys.executable)`
    (embedded EXE icon resource) before path-based icon fallback.
- `scripts/ci/build-windows-decode-bin.py`
  - Added `--add-data` entries for `tape-decode-rust.ico` as:
    - `resources/icon/tape-decode-rust.ico`
    - `tape-decode-rust.ico`
    - `decode-rust-gui.ico`
- `.github/workflows/build_windows_decode.yml`
  - Added step `Verify bundled Windows icon payload + embedded EXE icon`.
- `.github/workflows/build.yml`
  - Added same verification step in the consolidated Windows job.

## Verification commands/results
- `python -m py_compile decode_launcher.py scripts/ci/build-windows-decode-bin.py`
  -> `PY_COMPILE_EXIT=0`
- Runtime probe (Python 3.12 + PyQt6, offscreen):
  - `ICON_PATH=...\\resources\\icon\\tape-decode-rust.ico`
  - `ICON_SUFFIX=.ico`
  - `ICON_IS_NULL=False`
  - `ICON_AVAILABLE_SIZES=[(16,16),(24,24),(32,32),(48,48),(64,64),(128,128),(256,256)]`
  - `THREADS_DEFAULT=26`
  - `MT_DISTANCE_DEFAULT=80`
  - probe exit `0`
- Launcher smoke run:
  - `APP_RUNNING_AFTER_8S=1`
  - then terminated intentionally for automation.

## Notes
- Existing untracked restore logs from prior work were left untouched:
  - `PROMPT_pyqt6_still_required_20260823.md`
  - `RESTOREPOINT_V4_SELFCONTAINED_20260826.md`
