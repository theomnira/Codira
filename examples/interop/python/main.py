"""Calls a Codira library from Python through ctypes.

No bindings, no build step, no extension module: the library exports plain C
symbols, so the standard library is enough.

    python main.py ../codira/target/mod.codiralib
"""

import ctypes
import sys
from pathlib import Path


def load(library_path: Path) -> ctypes.CDLL:
    lib = ctypes.CDLL(str(library_path))

    # ctypes defaults every argument and return to `int`, which silently
    # truncates doubles and 64-bit integers on most platforms. Declaring the
    # signatures is not optional -- it is the contract the `.code` source's
    # `@export("C")` declared on the other side.
    lib.codira_add.argtypes = [ctypes.c_int64, ctypes.c_int64]
    lib.codira_add.restype = ctypes.c_int64

    lib.codira_scale.argtypes = [ctypes.c_double, ctypes.c_double]
    lib.codira_scale.restype = ctypes.c_double

    lib.codira_sum_of_squares.argtypes = [ctypes.c_double, ctypes.c_double]
    lib.codira_sum_of_squares.restype = ctypes.c_double

    lib.codira_length_squared.argtypes = [ctypes.c_double, ctypes.c_double]
    lib.codira_length_squared.restype = ctypes.c_double

    lib.codira_scaled_length_squared.argtypes = [ctypes.c_double] * 3
    lib.codira_scaled_length_squared.restype = ctypes.c_double

    lib.codira_div.argtypes = [ctypes.c_int64, ctypes.c_int64]
    lib.codira_div.restype = ctypes.c_int64

    lib.codira_mod.argtypes = [ctypes.c_int64, ctypes.c_int64]
    lib.codira_mod.restype = ctypes.c_int64

    return lib


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <path to .codiralib>", file=sys.stderr)
        return 2

    lib = load(Path(sys.argv[1]).resolve())

    checks = [
        ("codira_add(20, 22)", lib.codira_add(20, 22), 42),
        ("codira_scale(1.5, 4.0)", lib.codira_scale(1.5, 4.0), 6.0),
        ("codira_sum_of_squares(3, 4)", lib.codira_sum_of_squares(3.0, 4.0), 25.0),
        ("codira_length_squared(3, 4)", lib.codira_length_squared(3.0, 4.0), 25.0),
        (
            "codira_scaled_length_squared(3, 4, 2)",
            lib.codira_scaled_length_squared(3.0, 4.0, 2.0),
            100.0,
        ),
        ("codira_div(47, 5)", lib.codira_div(47, 5), 9),
        ("codira_mod(47, 5)", lib.codira_mod(47, 5), 2),
    ]

    failures = 0
    for label, actual, expected in checks:
        ok = actual == expected
        failures += not ok
        print(f"  {'ok  ' if ok else 'FAIL'} {label} = {actual}"
              + ("" if ok else f" (expected {expected})"))

    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
