#!/usr/bin/env python3
"""Generate the formula and signed APT repository from the release's native packages."""
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile

version = sys.argv[1].removeprefix("v")
assert re.fullmatch(r"\d+\.\d+\.\d+(?:-(?:alpha|beta|rc|test)\.\d+)?", version)
dist = Path("dist")
repo = os.environ["GITHUB_REPOSITORY"]
base = f"https://github.com/{repo}/releases/download/v{version}"
formula = ['class Ballast < Formula', '  desc "Keep your machine responsive while coding agents work"',
           f'  homepage "https://github.com/{repo}"', f'  version "{version}"',
           '  license any_of: ["MIT", "Apache-2.0"]', '  depends_on :macos', '  depends_on macos: :big_sur', '']
for architecture, target in [("arm", "aarch64-apple-darwin"), ("intel", "x86_64-apple-darwin")]:
    name = f"ballast-{version}-{target}.tar.gz"
    digest = hashlib.sha256((dist / name).read_bytes()).hexdigest()
    formula.extend([f'  on_{architecture} do', f'    url "{base}/{name}"', f'    sha256 "{digest}"', '  end', ''])
formula.extend(['  def install', '    bin.install "ballast"', '    doc.install "README.md"', '  end', '',
                '  def caveats', '    <<~EOS', '      Run `ballast install` once to start the user service and install agent hooks.',
                '      Open Codex /hooks to approve the hooks. Upgrades restart Ballast automatically.',
                '      Before `brew uninstall ballast`, run `ballast uninstall` to remove the service and hooks.',
                '    EOS', '  end', '', '  test do', '    assert_match version.to_s, shell_output("#{bin}/ballast --version")', '  end', 'end'])
(dist / "ballast.rb").write_text("\n".join(formula) + "\n")
site = Path("site")
site.mkdir(exist_ok=True)
# Carry older packages forward so pinned versions remain installable after publication.
previous = Path("previous/apt-repository.tar.gz")
if previous.exists():
    with tarfile.open(previous) as archive:
        archive.extractall(site, filter="data")
pool = site / "apt/pool/main/b/ballast"
pool.mkdir(parents=True, exist_ok=True)
for package in dist.glob("*.deb"):
    shutil.copy2(package, pool / package.name)
for arch in ["amd64", "arm64"]:
    index = site / f"apt/dists/stable/main/binary-{arch}"
    index.mkdir(parents=True, exist_ok=True)
    data = subprocess.check_output(["dpkg-scanpackages", "--multiversion", "--arch", arch, "pool"], cwd=site / "apt")
    (index / "Packages").write_bytes(data)
    (index / "Packages.gz").write_bytes(gzip.compress(data, mtime=0))
release_dir = site / "apt/dists/stable"
for name in ["Release", "Release.gpg", "InRelease"]:
    (release_dir / name).unlink(missing_ok=True)
release = subprocess.check_output(["apt-ftparchive", "-o", "APT::FTPArchive::Release::Origin=Ballast",
    "-o", "APT::FTPArchive::Release::Label=Ballast", "-o", "APT::FTPArchive::Release::Suite=stable",
    "-o", "APT::FTPArchive::Release::Codename=stable", "-o", "APT::FTPArchive::Release::Architectures=amd64 arm64",
    "-o", "APT::FTPArchive::Release::Components=main", "release", "."], cwd=release_dir)
(release_dir / "Release").write_bytes(release)
for flags, output in [(["--clearsign"], "InRelease"), (["--armor", "--detach-sign"], "Release.gpg")]:
    subprocess.run(["gpg", "--batch", "--yes", "--local-user", os.environ["APT_SIGNING_FINGERPRINT"],
                    "--output", str(release_dir / output), *flags, str(release_dir / "Release")], check=True)
shutil.copy2("packaging/key.gpg", site / "key.gpg")
subprocess.run(["gpg", "--verify", str(release_dir / "InRelease")], check=True)
(site / ".nojekyll").touch()
with tarfile.open(dist / "apt-repository.tar.gz", "w:gz") as archive:
    for path in sorted(site.iterdir()):
        archive.add(path, arcname=path.name)
(dist / "SHA256SUMS").write_text("".join(
    f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
    for path in sorted(dist.iterdir()) if path.name != "SHA256SUMS"))
