#!/usr/bin/env python3
"""Report Rust file boundaries and function signatures using ast-grep."""

import argparse
import json
import re
import shutil
import subprocess
from collections import Counter
from pathlib import Path


def scan(root, kind):
    executable = shutil.which("ast-grep")
    if executable is None:
        raise RuntimeError("Install ast-grep to inspect Rust syntax.")
    rule = f"id: inventory\nlanguage: Rust\nrule:\n  kind: {kind}"
    result = subprocess.run(
        [executable, "scan", "--inline-rules", rule, "--json=stream", "src"],
        cwd=root, text=True, capture_output=True,
    )
    if result.returncode not in (0, 1):
        raise RuntimeError(result.stderr)
    try:
        return [json.loads(line) for line in result.stdout.splitlines() if line]
    except json.JSONDecodeError as error:
        raise RuntimeError("ast-grep returned invalid JSON") from error


def inventory(root):
    content = {
        path.relative_to(root).as_posix(): path.read_text()
        for path in sorted((root / "src").rglob("*.rs"))
    }
    imports = scan(root, "use_declaration")
    modules = scan(root, "mod_item")
    functions = scan(root, "function_item")
    owners = {}
    dependencies = set()
    for item in modules:
        declaration = item["text"]
        if not declaration.rstrip().endswith(";"):
            continue
        name = re.search(r"\bmod\s+(\w+)\s*;", declaration)
        if name is None:
            continue
        source = Path(item["file"]).as_posix()
        before = content[source].encode()[:item["range"]["byteOffset"]["start"]].decode()
        attributes = re.search(r"((?:\s*#\[[^\]]*\])+\s*)$", before)
        override = re.findall(r'#\[path\s*=\s*"([^"]+)"\]', attributes[0]) if attributes else []
        target = (Path(source).parent / (override[-1] if override else name[1] + ".rs")).as_posix()
        if target in content:
            owners[target] = source
            dependencies.add((source, target))
    for item in imports:
        source = Path(item["file"]).as_posix()
        for name in re.findall(r"crate::(\w+)", item["text"]):
            target = "src/" + name + ".rs"
            if target in content and source != target:
                dependencies.add((source, target))
        if re.search(r"\bsuper\b", item["text"]) and source in owners:
            dependencies.add((source, owners[source]))
    incoming = Counter(target for _, target in dependencies)
    return {
        "coverage": {
            "files": len(content),
            "lines": sum(len(text.splitlines()) for text in content.values()),
            "functions": len(functions),
        },
        "limits": (
            "Syntax-level file graph. Does not resolve grouped crate imports, aliases, "
            "inline module ownership, cfg selection or calls. Not dead-code evidence."
        ),
        "files": [
            {"path": path, "lines": len(text.splitlines()), "owner": owners.get(path),
             "incoming": incoming[path]}
            for path, text in content.items()
        ],
        "dependencies": [{"from": source, "to": target} for source, target in sorted(dependencies)],
        "skeleton": [
            {"path": item["file"], "line": item["range"]["start"]["line"] + 1,
             "signature": item["text"].split("{", 1)[0].strip()}
            for item in sorted(functions, key=lambda item: (item["file"], item["range"]["start"]["line"]))
        ],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--format", choices=["json", "dot", "summary", "skeleton"], default="summary")
    arguments = parser.parse_args()
    report = inventory(arguments.root.resolve())
    if arguments.format == "json":
        print(json.dumps(report, indent=2))
    elif arguments.format == "dot":
        print("digraph rust_files {")
        for edge in report["dependencies"]:
            print(f'  {json.dumps(edge["from"])} -> {json.dumps(edge["to"])};')
        print("}")
    elif arguments.format == "skeleton":
        for item in report["skeleton"]:
            print(f'{item["path"]}:{item["line"]} {item["signature"]}')
    else:
        print(json.dumps(report["coverage"]))
        print(report["limits"])
        for item in sorted(report["files"], key=lambda item: (-item["incoming"], item["path"]))[:25]:
            print(f'{item["incoming"]:3} incoming {item["lines"]:4} lines '
                  f'{item["path"]} owner={item["owner"]}')


if __name__ == "__main__":
    main()
