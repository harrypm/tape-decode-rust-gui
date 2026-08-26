# PROMPT — GUI MT defaults update (20260826)

User request: set GUI `MT Distance Size` stock default from 60 to 80, and set
GUI default threads to a high-side rounded value around 80% of system threads
(logical CPUs), not physical cores.

## Inputs
- "update MT distance to 80 and set threads to 80% of system threads by stock"
- Clarification: "threads not cores*"
- Clarification: "MT Distance Size is 60 by stock, set to 80 for GUI, round threads to nearest high number of 80% or so of system cores"

## Commands run
- ripgrep across repo for mt/thread flags and GUI wiring.
- read `crates/tape-decode-cli/src/cli.rs` and `decode_launcher.py`.
- `python -m py_compile decode_launcher.py` -> `PY_COMPILE_EXIT=0`.

## Hard findings
- CLI defaults are in `crates/tape-decode-cli/src/cli.rs`:
  `--mt-threads` default 0, `--mt-distance-size` default 20.
- GUI defaults are in `decode_launcher.py` and differed from CLI:
  `threads_spin` default 4, `mt_distance_size_spin` default 60.
- Request targets GUI behavior ("for GUI"), so only launcher defaults changed.

## Changes made
- `decode_launcher.py`
  - Added helper `_default_mt_threads()`:
    - Reads logical thread count via `os.cpu_count()`.
    - Computes high-side rounded ~80% default using integer ceil logic:
      `(system_threads * 8 + 9) // 10`.
  - Updated GUI thread spinbox:
    - range: `0..max(64, os.cpu_count() or 1)` (prevents clipping on high-thread hosts).
    - default value: `_default_mt_threads()`.
  - Updated GUI MT distance default:
    - `mt_distance_size_spin.setValue(80)` (was 60).

## Validation
- `decode_launcher.py` compiles cleanly (`py_compile` exit 0).

## Follow-up verification (user request: run app + confirm defaults)
- First runtime probe using PATH `python` failed as expected on this host:
  `ModuleNotFoundError: No module named 'PyQt6'` (PATH python is MSYS without PyQt6).
- Re-ran probe using `C:\Users\Harry\AppData\Local\Programs\Python\Python312\python.exe`
  and instantiated `DecodeLauncherWindow` offscreen to read live widget defaults:
  - `SYSTEM_THREADS=32`
  - `EXPECTED_THREADS_DEFAULT=26` (ceil(0.8 * 32))
  - `ACTUAL_THREADS_DEFAULT=26` ✅
  - `THREAD_SPIN_MIN=0`
  - `THREAD_SPIN_MAX=64`
  - `ACTUAL_MT_DISTANCE_DEFAULT=80` ✅
- Application launch smoke test (same Python 3.12 interpreter):
  - Started `decode.py`, waited 8s.
  - `APP_RUNNING_AFTER_8S=1` (PID observed), then intentionally terminated for automation.
  - Confirms GUI process starts and remains running with updated defaults.
