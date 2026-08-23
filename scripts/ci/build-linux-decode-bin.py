#!/usr/bin/env python3
from __future__ import annotations

import os
import shutil
import tempfile
from pathlib import Path

import PyInstaller.__main__

os.environ.setdefault("SETUPTOOLS_RUST_CARGO_PROFILE", "release")


_LEVELS: tuple[str, ...] = ("x86-64-v1", "x86-64-v2", "x86-64-v3", "x86-64-v4")
# Triples we may produce level builds for on Linux CI
_TRIPLES: tuple[str, ...] = ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu")


def _binary_name() -> str:
    return "tape-decode.exe" if os.name == "nt" else "tape-decode"


def _platform_sep() -> str:
    return ";" if os.name == "nt" else ":"


def _resolve_tape_decode_bin() -> Path:
    explicit = os.environ.get("TAPE_DECODE_BIN", "").strip()
    if explicit:
        p = Path(explicit)
        if p.is_file():
            return p.resolve()
    bin_name = _binary_name()
    repo_root = Path.cwd()
    # Prefer a level build (lowest first = v1) as the default root binary.
    # This ensures the bare "." binary inside the bundle is always runnable
    # on any x86-64 host (CI verification and end-users on older CPUs).
    for lvl in _LEVELS:
        for tri in _TRIPLES:
            p = repo_root / f"target-{lvl}" / tri / "release" / bin_name
            if p.is_file():
                return p.resolve()
    # Legacy single-build locations and generic target/
    candidates = [
        Path("target/x86_64-unknown-linux-gnu/release/tape-decode"),
        Path("target/aarch64-unknown-linux-gnu/release/tape-decode"),
        Path("target/release/tape-decode"),
    ]
    for candidate in candidates:
        if candidate and candidate.is_file():
            return candidate.resolve()
    raise FileNotFoundError(
        "Could not find tape-decode Linux binary. Build it before running this packaging script."
    )


def _discover_level_binaries() -> list[tuple[Path, str]]:
    """Find per-level optimized binaries laid out as target-x86-64-vN/<triple>/release/...

    Returns (src_path, dest_rel) suitable for --add-binary.
    """
    bin_name = _binary_name()
    repo_root = Path.cwd()
    results: list[tuple[Path, str]] = []
    for lvl in _LEVELS:
        for tri in _TRIPLES:
            p = repo_root / f"target-{lvl}" / tri / "release" / bin_name
            if p.is_file():
                dest = f"target-{lvl}/{tri}/release/{bin_name}"
                results.append((p.resolve(), dest))
                break  # only one triple per level per job
    return results


def _discover_glvnd_libs() -> list[tuple[str, str]]:
    """Find libglvnd GL shared libs on the host for bundling.

    Qt6Gui links to libEGL.so.1 / libGL.so.1 (provided by libglvnd via the
    libegl1 / libgl1 packages). PyInstaller does not bundle system GL libs,
    so we add them explicitly. libGL.so.1 also needs libGLdispatch.so and
    libGLX.so.0 (also from libglvnd), so include those too. Returns
    (real_file_path, soname) pairs; symlinks are resolved so the staged copy
    carries the SONAME the dynamic loader looks for.
    """
    sonames = ("libEGL.so.1", "libGL.so.1", "libGLdispatch.so", "libGLX.so.0")
    search_dirs: list[str] = []
    for root in ("/usr/lib", "/lib"):
        search_dirs.append(root)
        try:
            for entry in os.listdir(root):
                full = os.path.join(root, entry)
                if os.path.isdir(full) and not os.path.islink(full):
                    search_dirs.append(full)
        except OSError:
            pass
    results: list[tuple[str, str]] = []
    seen: set[str] = set()
    for soname in sonames:
        for d in search_dirs:
            cand = os.path.join(d, soname)
            if os.path.exists(cand):
                real = os.path.realpath(cand)
                if real in seen:
                    continue
                seen.add(real)
                results.append((real, soname))
                break
    return results


def main() -> None:
    tape_decode_bin = _resolve_tape_decode_bin()
    print(f"Bundling default {tape_decode_bin}")

    pyi_args: list[str] = [
        "decode.py",
        "--collect-all",
        "PyQt6",
        "--collect-all",
        "PyQt6-Qt6",
        "--hidden-import",
        "decode_launcher",
        "--hidden-import",
        "decode_runtime",
        "--hidden-import",
        "decode_selftest",
        "--add-binary",
        f"{tape_decode_bin}{_platform_sep()}.",
        "--add-data",
        f"crates/tape-decode-cli/src/profiles/profiles.json{_platform_sep()}.",
        # Bundle icon assets so _resolve_icon_path can find them inside onefile bundles
        "--add-data",
        f"resources/icon/tape-decode-rust-256.png{_platform_sep()}resources/icon/tape-decode-rust-256.png",
        "--add-data",
        f"resources/icon/tape-decode-rust-256.png{_platform_sep()}tape-decode-rust-256.png",
        "--add-data",
        f"resources/icon/tape-decode-rust-256.png{_platform_sep()}decode-rust-gui.png",
        "--icon",
        "resources/icon/tape-decode-rust-256.png",
        "--onefile",
        "--name",
        "decode-rust-gui",
    ]

    # Explicitly ensure Qt platform plugins (especially libqxcb.so) are inside the bundle
    # so that setting QT_QPA_PLATFORM_PLUGIN_PATH at runtime can find them.
    try:
        import PyQt6  # type: ignore
        qt_root = os.path.join(os.path.dirname(PyQt6.__file__), "Qt6")
        plugins_dir = os.path.join(qt_root, "plugins")
        if os.path.isdir(plugins_dir):
            pyi_args += ["--add-data", f"{plugins_dir}{_platform_sep()}PyQt6/Qt6/plugins"]
            print(f"Adding Qt plugins from {plugins_dir}")
    except Exception as exc:
        print(f"Could not locate PyQt6 Qt plugins for collection: {exc}")

    # Bundle libglvnd GL shared libs (libEGL.so.1, libGL.so.1 + their internal
    # deps libGLdispatch.so, libGLX.so.0) so the Qt6 GUI lib can load on hosts
    # without mesa installed. PyInstaller does not bundle system GL libs by
    # default, so without this the Qt platform plugin fails to initialize.
    # Stage each as its SONAME so the dynamic loader finds it in _MEIPASS.
    glvnd_libs = _discover_glvnd_libs()
    if glvnd_libs:
        staging = Path(tempfile.mkdtemp(prefix="tape-decode-glvnd-"))
        for real_path, soname in glvnd_libs:
            staged = staging / soname
            shutil.copy2(real_path, staged)
            print(f"Bundling GL lib {real_path} -> {soname}")
            pyi_args += ["--add-binary", f"{staged}{_platform_sep()}."]
    else:
        print("WARNING: no libglvnd GL libs found; bundle may not be self-contained on hosts without mesa")

    for src, dest in _discover_level_binaries():
        print(f"Bundling level binary {src} -> {dest}")
        pyi_args += ["--add-binary", f"{src}{_platform_sep()}{dest}"]

    PyInstaller.__main__.run(pyi_args)


if __name__ == "__main__":
    main()
