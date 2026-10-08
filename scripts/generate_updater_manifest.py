"""Build Tauri's static manifest from the exact signed assets we publish."""
import argparse
import json
import re
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import quote

TARGETS = {
    "linux-x86_64": ("x86_64-unknown-linux-gnu", ".AppImage"),
    "darwin-x86_64": ("x86_64-apple-darwin", ".app.tar.gz"),
    "darwin-aarch64": ("aarch64-apple-darwin", ".app.tar.gz"),
    "windows-x86_64-nsis": ("x86_64-pc-windows-msvc", "-setup.exe"),
    "windows-x86_64-msi": ("x86_64-pc-windows-msvc", ".msi"),
}


def manifest(directory: Path, repository: str, tag: str, notes: str = "") -> dict:
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", tag):
        raise ValueError(f"Not a release version: {tag}")
    base = f"https://github.com/{repository}/releases/download/{quote(tag, safe='')}"
    platforms = {}
    for platform, (target, suffix) in TARGETS.items():
        name = f"3dam-{tag}-{target}{suffix}"
        asset = directory / name
        if not asset.is_file() or asset.stat().st_size == 0:
            raise ValueError(f"Missing or empty updater artifact: {asset}")
        signature = (directory / f"{name}.sig").read_text().strip()
        if not signature:
            raise ValueError(f"Empty signature: {name}.sig")
        platforms[platform] = {"url": f"{base}/{quote(name, safe='')}", "signature": signature}
    # The installer-specific keys ensure an MSI installation stays MSI and NSIS stays NSIS.
    return {
        "version": tag.removeprefix("v"),
        "notes": notes or f"Release details: https://github.com/{repository}/releases/tag/{tag}",
        "pub_date": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        "platforms": platforms,
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, default=Path("."))
    parser.add_argument("--repository", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--notes", type=Path)
    parser.add_argument("--output", type=Path, default=Path("latest.json"))
    args = parser.parse_args()
    result = manifest(args.directory, args.repository, args.tag,
                      args.notes.read_text() if args.notes else "")
    args.output.write_text(json.dumps(result, indent=2) + "\n")
