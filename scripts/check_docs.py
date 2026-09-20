#!/usr/bin/env python3
"""Check this repository's Markdown conventions with the Python standard library.

Checks local inline links/ATX anchors, fenced JSON examples, and fence closure.
External URLs, reference-style links, HTML anchors, and Mermaid rendering are
outside this small checker's scope. No network requests are made.
"""

import json
import re
import subprocess
import sys
import unicodedata
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
LINK = re.compile(r"\[[^\]\n]*\]\(([^\s)]+)\)")
FENCE = re.compile(r"^\s{0,3}(`{3,}|~{3,})(.*)$")


def slug(text):
    text = re.sub(r"\[([^]]+)\]\([^)]+\)", r"\1", text)
    return "".join(
        char for char in text.lower()
        if unicodedata.category(char)[0] in "LN" or char in " _-"
    ).replace(" ", "-")


def scan(path):
    """Return anchors, prose lines, and errors, excluding fenced code from prose."""
    anchors, prose, errors = set(), [], []
    marker, language, body, start = None, "", [], 0
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        fence = FENCE.match(line)
        if marker:
            if fence and fence[1][0] == marker[0] and len(fence[1]) >= len(marker) and not fence[2].strip():
                example = "\n".join(body)
                try:
                    if language == "json":
                        json.loads(example)
                    elif language == "http" and "\n\n" in example:
                        payload = example.split("\n\n", 1)[1].strip()
                        if payload.startswith(("{", "[")):
                            json.loads(payload)
                except json.JSONDecodeError as exc:
                    errors.append(f"{path}:{start}: invalid JSON example: {exc}")
                marker = None
            else:
                body.append(line)
            continue
        if fence:
            marker, language, body, start = fence[1], fence[2].strip(), [], number
            continue
        prose.append((number, line))
        heading = re.match(r"^#{1,6}\s+(.+?)\s*#*\s*$", line)
        if heading:
            base = slug(heading[1])
            anchor, count = base, 0
            while anchor in anchors:
                count += 1
                anchor = f"{base}-{count}"
            anchors.add(anchor)
    if marker:
        errors.append(f"{path}:{start}: unclosed code fence")
    return anchors, prose, errors


def check(root, paths):
    documents = {path.resolve(): scan(path) for path in paths}
    errors = [error for _, _, found in documents.values() for error in found]
    for path, (_, prose, _) in documents.items():
        for number, line in prose:
            # Literal inline code is not a link, even if it resembles one.
            line = re.sub(r"(`+).*?\1", "", line)
            for target in LINK.findall(line):
                url = urlsplit(target.strip("<>"))
                if url.scheme or url.netloc:
                    continue
                relative = unquote(url.path)
                destination = (root / relative.lstrip("/") if relative.startswith("/")
                               else path.parent / relative) if relative else path
                destination = destination.resolve()
                if not destination.is_relative_to(root.resolve()):
                    errors.append(f"{path}:{number}: link leaves repository: {target}")
                elif not destination.exists():
                    errors.append(f"{path}:{number}: missing local target: {target}")
                elif url.fragment and destination.suffix.lower() == ".md":
                    anchors = documents.get(destination)
                    if anchors is None:
                        anchors = scan(destination)
                    if unquote(url.fragment) not in anchors[0]:
                        errors.append(f"{path}:{number}: missing local anchor: {target}")
    return errors


def main():
    names = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z", "--", "*.md"],
        cwd=ROOT,
    ).decode().split("\0")
    paths = sorted({ROOT / name for name in names if name and (ROOT / name).is_file()})
    errors = check(ROOT, paths)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"Documentation checks passed ({len(paths)} Markdown files).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
