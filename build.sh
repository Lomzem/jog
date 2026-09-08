#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
DIST_DIR="${ROOT_DIR}/dist"

[[ "$(uname -s)" == "Linux" ]] || {
    echo "error: release builds require a Linux host" >&2
    exit 1
}
command -v docker >/dev/null 2>&1 || {
    echo "error: Docker is not installed" >&2
    exit 1
}
docker info >/dev/null 2>&1 || {
    echo "error: Docker is not available; start the Docker daemon" >&2
    exit 1
}

version="$(sed -n '/^\[package\]$/,/^\[/{s/^version = "\([^"]*\)"$/\1/p;}' "${ROOT_DIR}/Cargo.toml")"
[[ -n "${version}" ]] || {
    echo "error: Cargo package version was not found" >&2
    exit 1
}

rm -rf -- "${DIST_DIR}"
mkdir -p -- "${DIST_DIR}"
DOCKER_BUILDKIT=1 docker build \
    --platform linux/amd64 \
    --target artifacts \
    --build-arg "PACKAGE_VERSION=${version}" \
    --output "type=local,dest=${DIST_DIR}" \
    "${ROOT_DIR}"

windows="${DIST_DIR}/jog-${version}-windows-x86_64.exe"
deb="${DIST_DIR}/jog_${version}_amd64.deb"
[[ -f "${windows}" && -f "${deb}" ]] || {
    echo "error: Docker did not produce the expected release artifacts" >&2
    exit 1
}
printf 'Created:\n  %s\n  %s\n' "${windows}" "${deb}"
