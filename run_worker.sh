#!/bin/bash -e

WD=$(realpath $(dirname $0))

export SBUILD_CONFIG=${SBUILD_CONFIG:-$WD/sbuildrc}
export AUTOPKGTEST=$WD/autopkgtest-wrapper

exec janitor-worker --tee "$@"
