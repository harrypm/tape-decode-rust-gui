#!/usr/bin/env python3
"""Self-contained bundle smoke test for decode-rust-gui.

Invoked via `decode-rust-gui --selftest` (handled in decode.py before CLI
passthrough). Proves the PyInstaller onefile bundle is self-contained:

1. PyQt6 bindings import and a QApplication can be constructed offscreen.
   This loads the Qt platform plugin (qwindows.dll / libqxcb.so /
   libqcocoa.dylib) from the bundle -- the exact failure mode that surfaces
   to users as "PyQt6 missing / Python required" when the plugin or the Qt6
   runtime DLLs are not bundled.
2. The bundled Rust `tape-decode` binary resolves and runs `list-profiles`,
   proving the decoder binary is packaged and executable.

Prints `SELFTEST OK` and exits 0 on success; exits non-zero with a clear
message on failure. Designed to run headless in CI via QT_QPA_PLATFORM=offscreen.
"""
from __future__ import annotations

import os
import sys


def _ensure_qt_plugin_path() -> None:
    """Point Qt at the bundled platform plugins before any PyQt6 import.

    Mirrors the frozen-bundle block in decode_launcher.py so the selftest
    (which does not import decode_launcher) exercises the same plugin
    discovery the real GUI relies on -- critical on Linux, where the
    offscreen/xcb plugins are only found via QT_QPA_PLATFORM_PLUGIN_PATH.
    """
    if not getattr(sys, "frozen", False):
        return
    base = getattr(sys, "_MEIPASS", None) or os.path.dirname(sys.executable)
    if not base:
        return
    for _rel in (
        os.path.join("PyQt6", "Qt6", "plugins"),
        os.path.join("PyQt6", "Qt", "plugins"),
        "plugins",
    ):
        _plug = os.path.join(base, _rel)
        if os.path.isdir(os.path.join(_plug, "platforms")):
            os.environ.setdefault(
                "QT_QPA_PLATFORM_PLUGIN_PATH", os.path.join(_plug, "platforms")
            )
            os.environ.setdefault("QT_PLUGIN_PATH", _plug)
            break


def run_selftest() -> int:
    os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
    _ensure_qt_plugin_path()
    failures: list[str] = []

    # 1) PyQt6 bindings + Qt platform plugin load.
    try:
        from PyQt6.QtCore import QCoreApplication  # noqa: F401
        from PyQt6.QtGui import QGuiApplication  # noqa: F401
        from PyQt6.QtWidgets import QApplication

        app = QApplication.instance() or QApplication(sys.argv)
        app.processEvents()
        app.quit()
    except Exception as exc:  # noqa: BLE001
        failures.append(f"PyQt6/Qt platform plugin: {exc}")

    # 2) Bundled Rust tape-decode binary resolves and runs.
    try:
        from decode_runtime import list_profiles

        profiles = list_profiles(timeout_seconds=30)
        if not profiles:
            failures.append("tape-decode list-profiles returned no profiles")
    except Exception as exc:  # noqa: BLE001
        failures.append(f"tape-decode binary: {exc}")

    if failures:
        for item in failures:
            print(f"SELFTEST FAIL: {item}", file=sys.stderr)
        print("SELFTEST FAIL", file=sys.stderr)
        return 1

    print("SELFTEST OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(run_selftest())
