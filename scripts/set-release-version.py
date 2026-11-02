#!/usr/bin/env python3
"""Point the install instructions at a release.

The landing page and the README show one concrete, copy-pasteable command
per package (`mousetouch_0.1.7-1_amd64.deb`, the download URL under
`releases/download/v0.1.7/`, ...). This rewrites every such version to the
one given, so the Release workflow can run it right after publishing and
commit the result; `--check` only reports whether anything would change.

    scripts/set-release-version.py 0.1.8
    scripts/set-release-version.py --check 0.1.8

Only versions that look like a release (`1.2.3`) are touched; the generic
`<version>` placeholders in the same files stay as they are.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FILES = [ROOT / "site" / "index.html", ROOT / "README.md"]
SEMVER = r"\d+\.\d+\.\d+"
# (pattern, replacement template) pairs; `{v}` is the new version.
RULES = [
    (rf"releases/download/v{SEMVER}/", "releases/download/v{v}/"),
    (rf"mousetouch_{SEMVER}-1_", "mousetouch_{v}-1_"),
    (rf"mousetouch-{SEMVER}-1\.", "mousetouch-{v}-1."),
    (rf"mousetouch-v{SEMVER}-", "mousetouch-v{v}-"),
]


def rewrite(text: str, version: str) -> str:
    for pattern, template in RULES:
        text = re.sub(pattern, template.format(v=version), text)
    return text


def main(argv: list[str]) -> int:
    check = "--check" in argv
    args = [a for a in argv if a != "--check"]
    if len(args) != 1 or not re.fullmatch(SEMVER, args[0]):
        print(__doc__.strip(), file=sys.stderr)
        return 2
    version = args[0]
    changed = []
    for path in FILES:
        old = path.read_text(encoding="utf-8")
        new = rewrite(old, version)
        if new != old:
            changed.append(path)
            if not check:
                path.write_text(new, encoding="utf-8")
    for path in changed:
        print(f"{'would update' if check else 'updated'} {path.relative_to(ROOT)}")
    if not changed:
        print(f"already at {version}")
    return 1 if (check and changed) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
