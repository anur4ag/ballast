#!/usr/bin/env python3
"""Prepare a tagged Cargo build, then package its native executable."""
import gzip
import io
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile

phase, tag, *args = sys.argv[1:]
if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-(?:alpha|beta|rc|test)\.\d+)?", tag):
    sys.exit("Expected vX.Y.Z or vX.Y.Z-{alpha,beta,rc,test}.N")
version = tag[1:]
if phase == "prepare":
    for file, pattern in [("Cargo.toml", r'(?m)^version = "[^"]+"'),
                          ("Cargo.lock", r'(?m)(name = "ballast"\n)version = "[^"]+"')]:
        path = Path(file)
        replacement = f'version = "{version}"'
        if file.endswith("lock"):
            replacement = r'\g<1>' + replacement
        path.write_text(re.sub(pattern, replacement, path.read_text(), count=1))
    sys.exit(0)

assert phase == "package"
target, = args
binary = Path(f"target/{target}/release/ballast")
subprocess.run([str(binary), "--version"], check=True)
epoch = int(subprocess.check_output(["git", "show", "-s", "--format=%ct"]).strip())
dist = Path("dist")
dist.mkdir(exist_ok=True)
# Normalize archive ownership and timestamps; gzip's filename and timestamp are also fixed.
with (dist / f"ballast-{version}-{target}.tar.gz").open("wb") as output:
    with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=epoch) as compressed:
        with tarfile.open(fileobj=compressed, mode="w") as archive:
            for source, name in [(binary, "ballast"), (Path("LICENSE-MIT"), "LICENSE-MIT"),
                                 (Path("LICENSE-APACHE"), "LICENSE-APACHE"), (Path("README.md"), "README.md")]:
                data = source.read_bytes()
                info = tarfile.TarInfo(name)
                info.size, info.mtime, info.mode = len(data), epoch, 0o755 if name == "ballast" else 0o644
                archive.addfile(info, io.BytesIO(data))
if "linux" in target:
    arch = "amd64" if target.startswith("x86_64") else "arm64"
    deb_version = version.replace("-", "~", 1)
    root = Path("deb-root")
    (root / "usr/bin").mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, root / "usr/bin/ballast")
    doc = root / "usr/share/doc/ballast"
    doc.mkdir(parents=True, exist_ok=True)
    for source in ["README.md", "LICENSE-MIT", "LICENSE-APACHE"]:
        shutil.copy2(source, doc / source)
    shutil.copytree("docs", doc / "docs", dirs_exist_ok=True)
    control = root / "DEBIAN"
    control.mkdir(exist_ok=True)
    # dpkg derives the actual native library requirements from the ELF binary.
    Path("debian").mkdir(exist_ok=True)
    Path("debian/control").write_text("Source: ballast\nMaintainer: Ballast maintainers <ballast@users.noreply.github.com>\n\nPackage: ballast\nArchitecture: any\nDescription: Memory guardian\n")
    dependencies = subprocess.check_output(["dpkg-shlibdeps", "-O", str(binary)], text=True).strip().split("=", 1)[1]
    (control / "control").write_text(f"""Package: ballast
Version: {deb_version}
Section: utils
Priority: optional
Architecture: {arch}
Maintainer: Ballast maintainers <ballast@users.noreply.github.com>
Homepage: https://github.com/anur4ag/ballast
Depends: {dependencies}
Description: Keep your machine responsive while coding agents work
 Run ballast install as your user to start the service and install hooks.
 Before removing the package, run ballast uninstall as that user.
""")
    subprocess.run(["dpkg-deb", "--root-owner-group", "--build", str(root),
                    str(dist / f"ballast_{deb_version}_{arch}.deb")], check=True,
                   env={**os.environ, "SOURCE_DATE_EPOCH": str(epoch)})
