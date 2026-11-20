#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-only
# Usage: packaging/test/run.sh <dist dir> <image>...
# Runs install-check.sh in a fresh container per image (needs network for
# the packages' dependencies).
set -eu
dist=$(cd "$1" && pwd)
shift
here=$(cd "$(dirname "$0")" && pwd)
for image in "$@"; do
    echo "== $image"
    docker run --rm -v "$dist:/dist:ro" -v "$here:/t:ro" "$image" sh /t/install-check.sh
done
