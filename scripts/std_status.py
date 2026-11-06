#!/usr/bin/env python3
"""The status column of std/STATUS.md, derived from the two std ratchets.

    python3 scripts/std_status.py           # print each module's status
    python3 scripts/std_status.py --check   # fail if std/STATUS.md disagrees

A module's status comes from the snapshots of
crates/codira_compiler/tests/stdlib_import_ratchet.rs and
crates/codira_compiler/tests/stdlib_check_ratchet.rs, and nothing else:

- importable: listed under "importable:" in the import snapshot (a program
  importing it checks, builds and loads);
- checks-but-not-importable: blocked in the import snapshot, but its file
  has no diagnostics when std/ is checked as one package with its prelude
  (check snapshot), so what stops it is what it imports or a prelude name;
- fails: blocked, and its own file has diagnostics in the check snapshot.

Rows of std/STATUS.md for modules that have no file yet carry `planned` or
`unsupported`; those are editorial and this script only checks that no
such row names a module that exists.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SNAPSHOTS = ROOT / "crates/codira_compiler/tests/snapshots"
IMPORT_SNAP = SNAPSHOTS / "stdlib_import_ratchet__stdlib_import_status.snap"
CHECK_SNAP = SNAPSHOTS / "stdlib_check_ratchet__stdlib_check_status.snap"
STATUS_MD = ROOT / "std/STATUS.md"

DERIVED = {"importable", "checks-but-not-importable", "fails"}
EDITORIAL = {"planned", "unsupported"}


def import_status():
    """{module: (importable, first_failure, needs)} from the import snapshot."""
    text = IMPORT_SNAP.read_text().split("---\n", 2)[2]
    modules = {}
    section = None
    current = None
    for line in text.splitlines():
        if line == "importable:":
            section = "importable"
        elif line == "blocked:":
            section = "blocked"
        elif section == "importable" and line.startswith("  std."):
            modules[line.strip()] = (True, "", "")
        elif section == "blocked" and line.startswith("std."):
            current = line.strip()
            modules[current] = (False, "", "")
        elif section == "blocked" and line.startswith("    needs: "):
            ok, first, _ = modules[current]
            modules[current] = (ok, first, line[len("    needs: "):])
        elif section == "blocked" and line.startswith("    "):
            ok, _, needs = modules[current]
            modules[current] = (ok, line.strip(), needs)
    return modules


def check_counts():
    """{module: semantic diagnostics of its own file} from the check snapshot."""
    counts = {}
    for line in CHECK_SNAP.read_text().splitlines():
        m = re.match(r"\s*(\d+) syntax\s+(\d+) semantic\s+(\S+)\.code$", line)
        if m:
            path = m.group(3)
            path = path[: -len("/mod")] if path.endswith("/mod") else path
            counts["std." + path.replace("/", ".")] = int(m.group(1)) + int(m.group(2))
    return counts


def derived():
    imports = import_status()
    counts = check_counts()
    out = {}
    for module, (ok, first, needs) in imports.items():
        if ok:
            status = "importable"
        elif counts.get(module, 1) == 0:
            status = "checks-but-not-importable"
        else:
            status = "fails"
        out[module] = (status, first, needs)
    return out


def status_rows():
    """{module: status} from the matrix tables of std/STATUS.md."""
    rows = {}
    for line in STATUS_MD.read_text().splitlines():
        m = re.match(r"\|\s*`(std\.[a-z0-9_.]+)`\s*\|\s*([a-z-]+)\s*\|", line)
        if m:
            if m.group(1) in rows:
                sys.exit(f"std/STATUS.md: two rows for {m.group(1)}")
            rows[m.group(1)] = m.group(2)
    return rows


def main():
    modules = derived()
    if "--check" not in sys.argv[1:]:
        for module in sorted(modules):
            status, first, needs = modules[module]
            print(f"{module}\t{status}\t{first}\t{needs}")
        totals = {}
        for status, _, _ in modules.values():
            totals[status] = totals.get(status, 0) + 1
        print(totals, file=sys.stderr)
        return 0
    rows = status_rows()
    errors = []
    for module, (status, _, _) in sorted(modules.items()):
        if module not in rows:
            errors.append(f"{module}: no row (status {status})")
        elif rows[module] != status:
            errors.append(f"{module}: row says {rows[module]}, snapshots say {status}")
    for module, status in sorted(rows.items()):
        if module in modules:
            continue
        if status in DERIVED:
            errors.append(f"{module}: no such module, but its row says {status}")
        elif status not in EDITORIAL:
            errors.append(f"{module}: unknown status {status}")
    for error in errors:
        print(f"std/STATUS.md: {error}", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
