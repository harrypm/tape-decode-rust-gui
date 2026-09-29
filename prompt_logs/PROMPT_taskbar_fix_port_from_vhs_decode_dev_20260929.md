# PROMPT: port vhs-decode-dev Linux taskbar fix to FLAC-Chop and tape-decode-rust (2026-09-29)

## Input
"apply my vhs-decode-dev taskbar fixes to flac-chop and to tape-decode-rust gui appimages as they both use the same QT6 base"

## Source fix (vhs-decode-dev, commit bf06c12e, restore_points/linux_taskbar_icon_fix_2026-09-29.md)
- Pin Qt app identity (applicationName + desktopFileName) after QApplication creation.
- `StartupWMClass=` in the AppImage .desktop file.
- AppRun exports RESOURCE_NAME and installs the user-level .desktop + hicolor icon (Exec = the AppImage file).

## Evidence gathered before editing (hard data)
- Both target AppRuns had no desktop integration.
- ~/.local/share/applications contained stray `Flac-Chop.cinnamon-generated.desktop` (Exec=build/gui/flac-chop)
  and `Decode-Rust-Gui.cinnamon-generated.desktop` (Exec=/tmp/.mount_decodeYgi9dZ/usr/bin/decode-rust-gui).
- Old v1.0.4 FLAC-Chop AppImage: WM_CLASS = "flac-chop", "FLAC-Chop" (already matched StartupWMClass) -> missing piece = installed .desktop.
- Running decode-rust-gui 4.0.0 AppImage: WM_CLASS = "decode-rust-gui", "decode-rust-gui" -> same conclusion.
- tape-decode-rust and FLAC-Chop differ from vhs-decode: FLAC-Chop is C++/Qt6, tape-decode-rust is PyQt6 (PyInstaller onefile in AppImage).

## Changes
FLAC-Chop
- gui/main.cpp: Linux-only RESOURCE_NAME=flac-chop (if unset) before QApplication; QGuiApplication::setDesktopFileName("flac-chop") after it. applicationName stays "FLAC-Chop" (== StartupWMClass).
- assets/appimage/flac-chop.desktop (new, moved out of the inline workflow heredoc).
- assets/appimage/AppRun (new): exports RESOURCE_NAME, installs ~/.local/share/applications/flac-chop.desktop + 512x512 icon on GUI launches only (not --probe/--version/CLI cuts), rewrites when the AppImage path/version changes, opt-out FLAC_CHOP_NO_INTEGRATION=1.
- .github/workflows/build.yml: copies the two files instead of inline heredocs.
tape-decode-rust
- decode_launcher.py: `_apply_linux_app_identity()` (Linux only) called right after QApplication creation.
- resources/appimage/decode-rust-gui.desktop: added StartupWMClass=decode-rust-gui.
- resources/appimage/AppRun: keeps the Qt plugin-path logic, adds RESOURCE_NAME + integration (only no-arg / launcher-alias GUI launches; skipped for `--selftest` and tape-decode CLI passthrough); opt-out DECODE_RUST_GUI_NO_INTEGRATION=1. File mode changed 644 -> 755.

## Commands / results
- bash -n / sh -n / dash -n on AppRuns: OK. py_compile decode_launcher.py: OK. build.yml YAML load: OK.
- Sandboxed HOME tests of both AppRuns (/tmp/appimage_integration_test): install OK, idempotent (mtime unchanged), CLI/selftest/APPIMAGE-unset/opt-out create 0 files, path with space and `%` handled, re-pointing to another AppImage rewrites Exec.
- desktop-file-validate: OK (only pre-existing "AudioVideo;Utility;" hint).
- xprop A/B, tape-decode-rust launcher from source, wrong argv0 (decode.py): old WM_CLASS "decode.py","decode.py"; new "decode-rust-gui","decode-rust-gui".
- FLAC-Chop built (cmake -S . -B /tmp/fc-build -G Ninja; cmake --build): OK. New binary with weird argv0: WM_CLASS "flac-chop","FLAC-Chop".
- End-to-end: real v1.0.4 AppDir + new AppRun + new binary, sandbox HOME: window WM_CLASS "flac-chop","FLAC-Chop"; flac-chop.desktop + icon installed.

## NOT yet verified (needs user)
- Real CI-built AppImages on the Linux Mint taskbar (icon, pinning, no new *.cinnamon-generated.desktop).
- No commit made, no restore point created (user has not yet confirmed "fixed").

## Follow-up prompt: "commit push and run test builds of both also make an overall dev note about this for future apps"
- Dev note written: ~/DEV_NOTE_linux_appimage_taskbar_integration.md (copies in FLAC-Chop/docs and tape-decode-rust/docs).
- Committed only the taskbar-fix files + dev note (unrelated untracked files left alone).
