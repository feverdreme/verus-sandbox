#!/bin/sh
# Builds the sandbox image on the current Docker context. On a Mac, start Colima first.
set -eu
cd "$(dirname "$0")"
docker build -t vsb:local .
