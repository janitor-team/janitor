#!/usr/bin/env python3

import argparse
import os
import pwd
import shlex
import subprocess

from janitor.config import get_distribution, read_config


def create_chroot(
    distro,
    sbuild_path,
    suites,
    sbuild_arch,
    include=[],  # noqa: B006
    setup_hooks=[],  # noqa: B006
):
    components = " ".join(distro.component)
    if distro.signed_by:
        options = f"[signed-by={distro.signed_by}] "
        deb_suites = [distro.name, *distro.extra]
        mirrors = [
            f"deb {options}{distro.archive_mirror_uri} {s} {components}"
            for s in deb_suites
        ]
    else:
        mirrors = [distro.archive_mirror_uri] + [
            f"deb {distro.archive_mirror_uri} {s} {components}" for s in distro.extra
        ]
    cmd = [
        "mmdebstrap",
        "--variant=buildd",
        distro.name,
        sbuild_path,
        *mirrors,
        "--mode=unshare",
        f"--arch={sbuild_arch}",
    ]
    cmd.append("--components=" + ",".join(distro.component))
    if include:
        cmd.append("--include=" + ",".join(include))
    if distro.signed_by:
        key = shlex.quote(distro.signed_by)
        # mmdebstrap passes the chroot root to setup hooks as $1.
        cmd.append(f'--setup-hook=install -D --mode=644 {key} "$1"{key}')

    for setup_hook in setup_hooks:
        cmd.append("--setup-hook=" + setup_hook)

    dirname, basename = os.path.split(sbuild_path)
    os.makedirs(dirname, exist_ok=True)

    print(shlex.join(cmd))
    subprocess.check_call(cmd)

    for suite in suites:
        link = os.path.join(dirname, f"{suite}-{sbuild_arch}-sbuild.tar.xz")
        if os.path.realpath(link) == os.path.realpath(sbuild_path):
            continue
        if os.path.islink(link):
            os.unlink(link)
        os.symlink(basename, link)


def get_sbuild_architecture():
    return (
        subprocess.check_output(["dpkg-architecture", "-qDEB_BUILD_ARCH"])
        .decode()
        .strip()
    )


parser = argparse.ArgumentParser()
parser.add_argument("--remove-old", action="store_true")
parser.add_argument(
    "--include",
    type=str,
    action="append",
    help="Include specified package.",
    default=[],
)
parser.add_argument(
    "--base-directory",
    type=str,
    help="Base directory for chroots",
    default=os.path.expanduser("~/.cache/sbuild"),
)
parser.add_argument("--user", type=str, help="User to create home directory for")
parser.add_argument(
    "--config", type=str, default="janitor.conf", help="Path to configuration."
)
parser.add_argument("distribution", type=str, nargs="*")
args = parser.parse_args()

try:
    with open(args.config) as f:
        config = read_config(f)
except FileNotFoundError:
    parser.error(f"config path {args.config} does not exist")

if not args.distribution:
    args.distribution = [d.name for d in config.distribution]

for distribution in args.distribution:
    try:
        distro_config = get_distribution(config, distribution)
    except KeyError:
        parser.error(f"no such distribution: {distribution}")

    sbuild_arch = get_sbuild_architecture()

    suites = []
    for campaign in config.campaign:
        if not campaign.debian_build:
            continue
        if campaign.debian_build.base_distribution != distro_config.name:
            continue
        suites.append(campaign.debian_build.build_distribution or campaign.name)
    sbuild_path = os.path.join(args.base_directory, distro_config.chroot + ".tar.xz")
    setup_hooks = []
    if args.user:
        setup_hooks.append(
            f"install -d --owner={args.user} {pwd.getpwnam(args.user).pw_dir}"
        )
    create_chroot(
        distro_config,
        sbuild_path,
        suites,
        sbuild_arch,
        args.include,
        setup_hooks=setup_hooks,
    )
