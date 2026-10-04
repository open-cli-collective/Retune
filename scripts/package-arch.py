#!/usr/bin/env python3
"""Package the validated Tauri Linux payload for the signed pacman repository."""
import argparse
import datetime
import io
import json
import pathlib
import re
import subprocess
import tarfile
import tempfile


def package(deb, version, arch, output, nfpm):
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("Expected the release's numeric version")
    if arch not in ("amd64", "arm64"):
        raise ValueError("Unsupported architecture")
    members = subprocess.check_output(["bsdtar", "-tf", str(deb)], text=True).splitlines()
    def member(prefix):
        matches = [name for name in members if name.startswith(prefix + ".tar.")]
        if len(matches) != 1:
            raise ValueError("Expected one " + prefix + " archive")
        return subprocess.check_output(["bsdtar", "-xOf", str(deb), matches[0]])
    with tarfile.open(fileobj=io.BytesIO(member("control")), mode="r:*") as control:
        entry = next(item for item in control.getmembers() if item.name.lstrip("./") == "control")
        fields = dict(line.split(": ", 1) for line in control.extractfile(entry).read().decode().splitlines() if ": " in line and not line.startswith(" "))
    if fields.get("Version") != version or fields.get("Architecture") != arch:
        raise ValueError("Debian payload version/architecture does not match the release")
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as directory:
        staging = pathlib.Path(directory)
        with tarfile.open(fileobj=io.BytesIO(member("data")), mode="r:*") as payload:
            payload.extractall(staging, filter="data")
        for required in ("usr/bin/retune-desktop", "usr/share/applications/Retune.desktop"):
            if not (staging / required).is_file():
                raise ValueError("Missing desktop payload: " + required)
        contents = [{"src": str(path), "dst": "/" + str(path.relative_to(staging))}
                    for path in sorted((staging / "usr").rglob("*")) if path.is_file()]
        contents.append({"src": str(pathlib.Path(__file__).resolve().parents[1] / "LICENSE"),
                         "dst": "/usr/share/licenses/retune/LICENSE"})
        config = {"name": "retune", "arch": arch, "platform": "linux", "version": version,
                  "release": "1", "section": "sound",
                  # Whole seconds avoid tar/MTREE timestamp rounding differences.
                  "mtime": datetime.datetime.fromtimestamp(entry.mtime, datetime.timezone.utc).isoformat(), "maintainer": "Open CLI Collective",
                  "description": "Desktop music player for local files and Spotify",
                  "homepage": "https://github.com/open-cli-collective/Retune", "license": "MIT",
                  "depends": ["webkit2gtk-4.1", "gtk3", "alsa-lib", "dbus", "gcc-libs", "glibc"],
                  "contents": contents}
        config_path = staging / "nfpm.json"
        config_path.write_text(json.dumps(config))
        destination = output / f"retune_{version}_linux_{arch}.pkg.tar.zst"
        subprocess.run([nfpm, "package", "--packager", "archlinux", "--config", str(config_path),
                        "--target", str(destination)], check=True)
        return destination


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("deb", type=pathlib.Path)
    parser.add_argument("version")
    parser.add_argument("arch", choices=("amd64", "arm64"))
    parser.add_argument("output", type=pathlib.Path)
    parser.add_argument("--nfpm", default="nfpm")
    args = parser.parse_args()
    print(package(args.deb, args.version, args.arch, args.output, args.nfpm))
