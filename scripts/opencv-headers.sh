#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Gundu Labs
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Build a minimal OpenCV sysroot for the `opencv` crate without installing
# opencv-devel.
#
# Fedora's opencv-devel hard-requires every OpenCV module, and opencv-viz drags
# in VTK, which drags in OpenCascade: 123 packages and ~751 MiB for a build that
# links exactly two libraries. The headers themselves are 8.9 MiB, so this
# fetches the -devel rpm, extracts only the headers, and points the unversioned
# link symlinks at the runtime libraries that are already installed.
#
# Result: `opencv-core` and `opencv-imgproc` are the only OpenCV packages the
# system needs, and they are runtime packages the daemon requires anyway.
#
# Usage:
#   scripts/opencv-headers.sh setup   # create/refresh the sysroot
#   scripts/opencv-headers.sh env     # print the cargo env vars (eval-able)
#   scripts/opencv-headers.sh clean   # remove the sysroot

set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
SYSROOT=${OPENCV_SYSROOT:-"$REPO/.opencv-sysroot"}

# Modules the crate is built against. Keep in sync with the `features` list on
# the `opencv` dependency in gaze-core/gaze/gaze-gui Cargo.toml. `core` is
# implicit in the crate but must be linked explicitly.
MODULES="core imgproc"

die() { printf 'opencv-headers: %s\n' "$*" >&2; exit 1; }

libdir() {
    if command -v rpm >/dev/null 2>&1; then
        rpm --eval '%{_libdir}'
    elif [ -d /usr/lib64 ]; then
        printf '/usr/lib64'
    else
        printf '/usr/lib'
    fi
}

setup() {
    command -v dnf >/dev/null 2>&1 || die \
        "needs dnf (Fedora/RHEL). On other distros install the OpenCV headers
       your package manager provides -- Debian's libopencv-dev and Arch's
       opencv do not have Fedora's OpenCascade problem."
    command -v rpm2cpio >/dev/null 2>&1 || die "needs rpm2cpio"
    command -v cpio >/dev/null 2>&1 || die "needs cpio (dnf install cpio)"

    LIBDIR=$(libdir)

    # Every module must already be present as a runtime library; otherwise the
    # link would fail later with a much less obvious error.
    for m in $MODULES; do
        ls "$LIBDIR"/libopencv_"$m".so.* >/dev/null 2>&1 || die \
            "libopencv_$m is not installed. Run:
       sudo dnf install opencv-core opencv-imgproc"
    done

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    printf 'fetching opencv-devel (headers only)...\n'
    dnf download opencv-devel --arch="$(uname -m)" --destdir="$tmp" >/dev/null 2>&1 \
        || die "dnf download opencv-devel failed"

    rpm=$(ls "$tmp"/opencv-devel-*.rpm 2>/dev/null | head -n1)
    [ -n "$rpm" ] || die "no opencv-devel rpm downloaded"

    rm -rf "$SYSROOT"
    mkdir -p "$SYSROOT/extract" "$SYSROOT/lib"
    (cd "$SYSROOT/extract" && rpm2cpio "$rpm" | cpio -idmu --quiet)

    inc=$(find "$SYSROOT/extract" -type d -name 'opencv4' -path '*/include/*' | head -n1)
    [ -n "$inc" ] || die "no include/opencv4 directory in $rpm"
    mv "$inc" "$SYSROOT/include"
    rm -rf "$SYSROOT/extract"

    # The rpm's unversioned .so symlinks are relative and would dangle here, so
    # repoint them at the installed runtime libraries.
    for m in $MODULES; do
        real=$(ls "$LIBDIR"/libopencv_"$m".so.* | sort | tail -n1)
        ln -sfn "$real" "$SYSROOT/lib/libopencv_$m.so"
    done

    printf 'sysroot ready: %s (%s)\n' "$SYSROOT" "$(du -sh "$SYSROOT/include" | cut -f1)"
}

# Emitted as `KEY=VAL` pairs for use as a command prefix. `environment` stays
# enabled -- it is the probe that reads these -- while the others are disabled so
# a stale system OpenCV cannot win over the sysroot.
env_vars() {
    [ -d "$SYSROOT/include" ] || die "no sysroot; run: just fetch-opencv-headers"

    links=$(printf '%s' "$MODULES" | tr ' ' '\n' | sed 's/^/opencv_/' | paste -sd, -)
    printf 'OPENCV_INCLUDE_PATHS=%s ' "$SYSROOT/include"
    printf 'OPENCV_LINK_PATHS=%s ' "$SYSROOT/lib"
    printf 'OPENCV_LINK_LIBS=%s ' "$links"
    printf 'OPENCV_DISABLE_PROBES=pkg_config,cmake,vcpkg_cmake,vcpkg'
}

case "${1:-}" in
setup) setup ;;
env) env_vars ;;
clean) rm -rf "$SYSROOT"; printf 'removed %s\n' "$SYSROOT" ;;
*) die "usage: $0 {setup|env|clean}" ;;
esac
