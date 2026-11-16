#!/usr/bin/env python3
"""Self-contained bundle smoke test for decode-rust-gui.

Invoked via `decode-rust-gui --selftest` (handled in decode.py before CLI
passthrough). Proves the PyInstaller onefile bundle is self-contained:

1. PyQt6 bindings import and a QApplication can be constructed offscreen.
   This loads the Qt platform plugin (qwindows.dll / libqxcb.so /
   libqcocoa.dylib) from the bundle -- the exact failure mode that surfaces
   to users as "PyQt6 missing / Python required" when the plugin or the Qt6
   runtime DLLs are not bundled.
2. The bundled Qt is isolated from the host: it searches for plugins only in
   the bundle (whatever QT_PLUGIN_PATH / QT_QPA_PLATFORM_PLUGIN_PATH the host
   sets), and programs the launcher starts get an environment with no trace
   of the bundle.
3. The bundled Rust `tape-decode` binary resolves and runs `list-profiles`,
   proving the decoder binary is packaged and executable.
4. CVBS single-output profile detection works through the bundled binary
   (`write-profile --out -`): CVBS format profiles (SMPTE Type C,
   Quadruplex, and the direct-composite SECAM variants -- single combined
   .tbc output) read back as composite, and a colour-under profile (VHS)
   does not.

Prints `SELFTEST OK` and exits 0 on success; exits non-zero with a clear
message on failure. Designed to run headless in CI via QT_QPA_PLATFORM=offscreen.
"""
from __future__ import annotations

import os
import sys
from pathlib import Path

from decode_runtime import bundle_dir, child_environment, isolate_qt_environment


def _isolation_failures(library_paths: list[str]) -> list[str]:
    """Check a frozen bundle against the host in both directions."""
    bundle = bundle_dir()
    if bundle is None:
        return []
    failures: list[str] = []

    # Qt adds the executable's own directory itself; everything else it
    # searches must be inside the bundle.
    allowed = (bundle.resolve(), Path(sys.executable).resolve().parent)
    for entry in library_paths:
        path = Path(entry).resolve()
        if not any(path == root or root in path.parents for root in allowed):
            failures.append(f"Qt searches for plugins outside the bundle: {entry}")

    roots = {str(bundle), str(bundle.resolve())}
    for name, value in child_environment().items():
        if name.startswith("_PYI_") or any(root in value for root in roots):
            failures.append(f"child process environment exposes the bundle: {name}={value}")
    return failures


def run_selftest() -> int:
    os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
    isolate_qt_environment()
    failures: list[str] = []

    # 1) PyQt6 bindings + Qt platform plugin load.
    library_paths: list[str] = []
    try:
        from PyQt6.QtCore import QCoreApplication
        from PyQt6.QtGui import QGuiApplication  # noqa: F401
        from PyQt6.QtWidgets import QApplication

        app = QApplication.instance() or QApplication(sys.argv)
        app.processEvents()
        library_paths = list(QCoreApplication.libraryPaths())
        app.quit()
    except Exception as exc:  # noqa: BLE001
        failures.append(f"PyQt6/Qt platform plugin: {exc}")

    # 2) Isolation of the bundled Qt from the host, and of the host from it.
    failures.extend(_isolation_failures(library_paths))

    # 3) Bundled Rust tape-decode binary resolves and runs.
    try:
        from decode_runtime import list_profiles

        profiles = list_profiles(timeout_seconds=30)
        if not profiles:
            failures.append("tape-decode list-profiles returned no profiles")
    except Exception as exc:  # noqa: BLE001
        failures.append(f"tape-decode binary: {exc}")

    # 4) CVBS single-output profile detection via the bundled binary.
    try:
        from decode_runtime import profile_is_composite

        cvbs_profile = None
        secam_cvbs_profile = None
        under_profile = None
        for name in profiles or []:
            upper = name.upper()
            if cvbs_profile is None and (
                "QUADRUPLEX" in upper or "TYPEC" in upper or "TYPEB" in upper or "VHD" in upper
            ):
                cvbs_profile = name
            if secam_cvbs_profile is None and upper.startswith("SECAM") and (
                "QUADRUPLEX" in upper or "TYPEC" in upper
            ):
                secam_cvbs_profile = name
            if under_profile is None and "VHS" in upper and not upper.startswith("SECAM"):
                under_profile = name
        if cvbs_profile is None:
            failures.append("no CVBS profile found in list-profiles output")
        elif profile_is_composite(profile=cvbs_profile, timeout_seconds=30) is not True:
            failures.append(f"CVBS detection: {cvbs_profile} not reported as composite")
        if secam_cvbs_profile is None:
            failures.append("no SECAM QUAD/TYPEC profile found in list-profiles output")
        elif profile_is_composite(profile=secam_cvbs_profile, timeout_seconds=30) is not True:
            failures.append(
                f"CVBS detection: {secam_cvbs_profile} not reported as single-output CVBS"
            )
        if under_profile is not None:
            if profile_is_composite(profile=under_profile, timeout_seconds=30) is not False:
                failures.append(f"CVBS detection: {under_profile} wrongly reported as composite")
    except Exception as exc:  # noqa: BLE001
        failures.append(f"CVBS profile detection: {exc}")

    if failures:
        for item in failures:
            print(f"SELFTEST FAIL: {item}", file=sys.stderr)
        print("SELFTEST FAIL", file=sys.stderr)
        return 1

    print("SELFTEST OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(run_selftest())
