#!/usr/bin/env python3
"""Plans the targets of the Publish workflow (.github/workflows/publish.yml).

Prints the build matrix as JSON on stdout, and a table of every candidate,
built or not, as Markdown on the file named by $GITHUB_STEP_SUMMARY.

Two groups of targets, on amd64 and arm64:

  nginx-org  nginx-module-mzstd, for the nginx.org packages of the stable and
             the mainline version. A distribution release is built for when
             nginx.org's repository for it has that exact version: the
             releases nginx.org has stopped updating keep an older one.
  distro     libnginx-mod-http-mzstd and nginx-mod-mzstd, for the nginx that
             the distribution itself ships: the Debian and Ubuntu releases of
             the first group, AlmaLinux 8 to 10 (the default AppStream
             stream) and the Fedora releases that Bodhi lists as current. The
             version is read from a container of the release.

To add a distribution, add it to NGINX_ORG_OSES or DISTRO_RPM_RELEASES (and
its Cloudsmith slug to CLOUDSMITH_DISTRO), and teach install-nginx.sh to set
it up if it uses neither apt nor dnf.

usage: publish-matrix.py [<stable-version>] [<mainline-version>]
Empty or missing versions are read from https://nginx.org/en/download.html.
"""

import concurrent.futures
import gzip
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request

NGINX_ORG = "https://nginx.org"

# The oldest NGINX the module builds against: src/bitfields.c reads
# r->main->terminated, which NGINX 1.25.4 added.
MIN_NGINX = (1, 25, 4)

# nginx.org's package repositories, by their directory under /packages/, and
# the container image of each release.
NGINX_ORG_OSES = {
    "debian": lambda release: f"debian:{release}",
    "ubuntu": lambda release: f"ubuntu:{release}",
    "rhel": lambda release: f"almalinux:{release}",
    "amzn": lambda release: f"amazonlinux:{release}",
}

# Cloudsmith's distribution slugs (`cloudsmith list distros deb|rpm`).
CLOUDSMITH_DISTRO = {"debian": "debian", "ubuntu": "ubuntu", "rhel": "el", "amzn": "amzn", "fedora": "fedora"}

# RHEL-compatible releases whose own nginx is packaged, as (os, release, image).
DISTRO_RPM_RELEASES = [("rhel", str(n), f"almalinux:{n}") for n in (8, 9, 10)]

ARCHES = {
    # Debian architecture: (RPM architecture, runner)
    "amd64": ("x86_64", "ubuntu-24.04"),
    "arm64": ("aarch64", "ubuntu-24.04-arm"),
}

REPOSITORY = {"stable": "ngx_mzstd", "mainline": "ngx_mzstd-mainline"}

REGISTRY = os.environ.get("REGISTRY", "public.ecr.aws/docker/library")


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": "ngx_mzstd-publish"})
    for attempt in range(4):
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                return response.read()
        except urllib.error.HTTPError as error:
            if error.code == 404:
                return None
            if attempt == 3:
                raise
        except urllib.error.URLError:
            if attempt == 3:
                raise


def version_tuple(version):
    return tuple(int(part) for part in version.split("."))


def latest_versions():
    """Returns the stable and mainline versions that nginx.org offers."""
    page = fetch(f"{NGINX_ORG}/en/download.html").decode()
    versions = {}
    for channel, heading in (("mainline", "Mainline version"), ("stable", "Stable version")):
        section = page.split(heading, 1)[1]
        versions[channel] = re.search(r"nginx-(\d+\.\d+\.\d+)\.tar\.gz", section).group(1)
    return versions


def subdirectories(url):
    listing = fetch(url)
    if listing is None:
        return []
    names = re.findall(r'href="([^"./][^"/]*)/"', listing.decode())
    return sorted(set(names))


def nginx_org_has(channel, os_name, release, arch, version):
    """Whether nginx.org's repository for a release has nginx `version`."""
    base = f"{NGINX_ORG}/packages/{'mainline/' if channel == 'mainline' else ''}{os_name}"
    if os_name in ("debian", "ubuntu"):
        index = fetch(f"{base}/dists/{release}/nginx/binary-{arch}/Packages.gz")
        if index is None:
            return False
        stanzas = gzip.decompress(index).decode(errors="replace").split("\n\n")
        return any(
            re.search(r"^Package: nginx$", stanza, re.M)
            and re.search(rf"^Version: {re.escape(version)}-", stanza, re.M)
            for stanza in stanzas
        )
    listing = fetch(f"{base}/{release}/{ARCHES[arch][0]}/RPMS/")
    if listing is None:
        return False
    return re.search(rf'href="nginx-{re.escape(version)}-[^"]*\.rpm"', listing.decode()) is not None


def nginx_org_releases(channel, os_name):
    if os_name in ("debian", "ubuntu"):
        return subdirectories(f"{NGINX_ORG}/packages/{'mainline/' if channel == 'mainline' else ''}{os_name}/dists/")
    # RHEL also lists 6Server and the like next to the plain release numbers.
    releases = subdirectories(f"{NGINX_ORG}/packages/{'mainline/' if channel == 'mainline' else ''}{os_name}/")
    return [release for release in releases if release.isdigit()]


def current_fedora_releases():
    data = json.loads(fetch("https://bodhi.fedoraproject.org/releases/?state=current&rows_per_page=100"))
    return sorted(
        {release["version"] for release in data["releases"] if release["id_prefix"] == "FEDORA"},
        key=int,
    )


PROBE = r"""
if command -v apt-get >/dev/null; then
    apt-get update -qq >/dev/null && apt-cache policy nginx | sed -n 's/^  Candidate: //p'
else
    dnf -q repoquery --latest-limit=1 --qf '%{version}\n' nginx 2>/dev/null | grep -m1 .
fi
"""


def distro_nginx_version(image):
    """Reads the version of nginx that a release's own repositories have."""
    for attempt in range(3):
        result = subprocess.run(
            ["docker", "run", "--rm", f"{REGISTRY}/{image}", "sh", "-c", PROBE],
            capture_output=True,
            text=True,
        )
        version = result.stdout.strip()
        if result.returncode == 0 and version and version != "(none)":
            # Debian versions carry an epoch and a revision: 1.26.3-3+deb13u9.
            return re.sub(r"-[^-]*$", "", re.sub(r"^\d+:", "", version))
        if attempt == 2:
            raise RuntimeError(f"cannot read the nginx version of {image}: {result.stderr.strip()}")


def target(group, os_name, release, image, arch, nginx, channel=None, codename=None):
    family = "deb" if os_name in ("debian", "ubuntu") else "rpm"
    distro = CLOUDSMITH_DISTRO[os_name]
    channel_part = f"{channel}-" if channel else ""
    return {
        "id": f"{group}-{channel_part}{distro}-{release}-{arch}",
        "name": f"{group} {channel_part}{distro}/{release} {arch} (nginx {nginx})",
        "group": group,
        "channel": channel or "",
        "family": family,
        "os": os_name,
        "release": release,
        "codename": codename or "",
        "image": image,
        "arch": arch,
        "runner": ARCHES[arch][1],
        "nginx": nginx,
        "repository": REPOSITORY[channel or "stable"],
        "distro": distro,
    }


def main():
    requested = (sys.argv[1:] + ["", ""])[:2]
    versions = latest_versions()
    for channel, version in zip(("stable", "mainline"), requested):
        if version:
            if not re.fullmatch(r"\d+\.\d+\.\d+", version):
                sys.exit(f"not an nginx version: {version!r}")
            versions[channel] = version

    targets = []
    report = []  # (target, nginx, verdict)
    pool = concurrent.futures.ThreadPoolExecutor(max_workers=16)

    # nginx.org packages.
    checks = {}
    for channel, version in versions.items():
        for os_name, image in NGINX_ORG_OSES.items():
            for release in nginx_org_releases(channel, os_name):
                for arch in ARCHES:
                    key = (channel, os_name, release, arch)
                    checks[key] = pool.submit(nginx_org_has, channel, os_name, release, arch, version)
    debian_like = set()
    for (channel, os_name, release, arch), check in sorted(checks.items()):
        version = versions[channel]
        if not check.result():
            continue
        if os_name in ("debian", "ubuntu"):
            debian_like.add((os_name, release))
        if version_tuple(version) < MIN_NGINX:
            report.append((f"nginx.org {channel} {os_name}/{release} {arch}", version, "unsupported nginx version"))
            continue
        image = NGINX_ORG_OSES[os_name](release)
        targets.append(target("nginx-org", os_name, release, image, arch, version, channel, release))
        report.append((f"nginx.org {channel} {os_name}/{release} {arch}", version, "build"))
    for channel in versions:
        if not any(t["channel"] == channel for t in targets):
            sys.exit(f"nginx.org has no packages of nginx {versions[channel]} ({channel}) yet")

    # The distributions' own nginx.
    candidates = [(os_name, release, f"{os_name}:{release}") for os_name, release in sorted(debian_like)]
    candidates += DISTRO_RPM_RELEASES
    candidates += [("fedora", release, f"fedora:{release}") for release in current_fedora_releases()]
    probes = {candidate: pool.submit(distro_nginx_version, candidate[2]) for candidate in candidates}
    for (os_name, release, image), probe in probes.items():
        version = probe.result()
        if version_tuple(version) < MIN_NGINX:
            minimum = ".".join(map(str, MIN_NGINX))
            report.append((f"distro {os_name}/{release}", version, f"skipped: older than {minimum}"))
            continue
        codename = release if os_name in ("debian", "ubuntu") else None
        for arch in ARCHES:
            targets.append(target("distro", os_name, release, image, arch, version, codename=codename))
        report.append((f"distro {os_name}/{release}", version, "build"))

    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as out:
            out.write(f"### Targets\n\nnginx.org stable {versions['stable']}, mainline {versions['mainline']}.\n\n")
            out.write("| Candidate | nginx | Plan |\n|---|---|---|\n")
            for name, version, verdict in report:
                out.write(f"| {name} | {version} | {verdict} |\n")
    for name, version, verdict in report:
        print(f"{name}: nginx {version}: {verdict}", file=sys.stderr)

    print(json.dumps(targets))


if __name__ == "__main__":
    main()
