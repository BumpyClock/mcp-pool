#!/usr/bin/env python3
"""Inventory project-name references without changing source or runtime state."""

import argparse
import collections
import json
import re
import subprocess
from pathlib import Path

PATTERN = r"mcp[-_. /]*pool"
BRAND = re.compile(PATTERN, re.IGNORECASE)
ENVIRONMENT = re.compile(r"\bMCP_POOL_[A-Z0-9_]+\b")


def git(root, *arguments, no_matches_ok=False):
    result = subprocess.run(
        ["git", "-C", str(root), *arguments], capture_output=True, check=False
    )
    if result.returncode != 0 and not (no_matches_ok and result.returncode == 1):
        raise RuntimeError(result.stderr.decode("utf-8", errors="replace").strip())
    return result.stdout


def audit(root, reference="HEAD", worktree=False):
    root = Path(git(root, "rev-parse", "--show-toplevel").decode().strip())
    revision = git(root, "rev-parse", "--verify", f"{reference}^{{commit}}").decode().strip()
    if worktree:
        paths = git(root, "ls-files", "-z").decode().split("\0")
        selection = []
    else:
        paths = git(root, "ls-tree", "-r", "--name-only", "-z", revision).decode().split("\0")
        selection = [revision]
    paths = sorted(set(filter(None, paths)))
    output = git(
        root, "grep", "--full-name", "--no-color", "-z", "-n", "-I", "-i", "-E",
        PATTERN, *selection, "--", no_matches_ok=True,
    )
    rows = {}
    variants = collections.Counter()
    environment = collections.defaultdict(set)
    for record in output.decode().splitlines():
        path, line, text = record.split("\0", 2)
        if not worktree:
            path = path.removeprefix(f"{revision}:")
        matches = BRAND.findall(text)
        number = int(line)
        row = rows.setdefault(path, {"path": path, "occurrences": 0, "lines": []})
        row["occurrences"] += len(matches)
        row["lines"].append({"number": number, "text": text})
        variants.update(matches)
        for name in ENVIRONMENT.findall(text):
            environment[name].add(f"{path}:{number}")
    return {
        "revision": revision,
        "source": "tracked working tree" if worktree else "committed tree",
        "pattern": PATTERN,
        "tracked_paths": len(paths),
        "matching_files": len(rows),
        "matching_lines": sum(len(row["lines"]) for row in rows.values()),
        "occurrences": sum(row["occurrences"] for row in rows.values()),
        "filename_matches": [path for path in paths if BRAND.search(path)],
        "variants": dict(sorted(variants.items())),
        "environment": {name: sorted(locations) for name, locations in sorted(environment.items())},
        "files": [rows[path] for path in sorted(rows)],
    }


def markdown(report):
    lines = [
        "| File | Occurrences | Matching line numbers |",
        "| --- | ---: | --- |",
    ]
    for row in report["files"]:
        numbers = ", ".join(str(line["number"]) for line in row["lines"])
        path = row["path"].replace("|", "\\|")
        lines.append(f"| `{path}` | {row['occurrences']} | {numbers} |")
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument("--ref", default="HEAD", help="Commit or branch to scan (default HEAD)")
    selection.add_argument("--worktree", action="store_true", help="Scan tracked working-tree contents")
    parser.add_argument("--format", choices=("summary", "json", "lines", "markdown"), default="summary")
    arguments = parser.parse_args()
    report = audit(arguments.root, arguments.ref, arguments.worktree)
    if arguments.format == "json":
        print(json.dumps(report, indent=2))
    elif arguments.format == "markdown":
        print(markdown(report))
    elif arguments.format == "lines":
        for row in report["files"]:
            for line in row["lines"]:
                print(f"{row['path']}:{line['number']}:{line['text']}")
    else:
        print(json.dumps({key: value for key, value in report.items()
                          if key not in {"files", "environment"}}, indent=2))


if __name__ == "__main__":
    main()
