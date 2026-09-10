set positional-arguments

# List available commands.
default:
    @just --list

# Tag HEAD with the Cargo version and push the tag to origin.
tag tag:
    #!/usr/bin/env bash
    set -euo pipefail
    tag="$1"
    version="$(sed -n '/^\[package\]$/,/^\[/{s/^version = "\([^"]*\)"$/\1/p;}' Cargo.toml)"
    if [[ -z "$version" || "$tag" != "v$version" ]]; then
        echo "error: tag must match Cargo.toml version: v$version" >&2
        exit 1
    fi
    if [[ -n "$(git status --porcelain)" ]]; then
        echo "error: commit or stash changes before tagging" >&2
        exit 1
    fi
    git check-ref-format "refs/tags/$tag"
    if git show-ref --verify --quiet "refs/tags/$tag"; then
        [[ "$(git rev-parse "refs/tags/$tag^{commit}")" == "$(git rev-parse HEAD)" ]] || {
            echo "error: existing tag points to a different commit" >&2
            exit 1
        }
    else
        git tag -a "$tag" -m "Release $tag"
    fi
    git push origin "refs/tags/$tag:refs/tags/$tag"

# Publish the three artifacts from a successful Build run for this tag.
release tag run_id:
    #!/usr/bin/env bash
    set -euo pipefail
    tag="$1"
    run_id="$2"
    git check-ref-format "refs/tags/$tag"
    [[ "$tag" == v* && "$run_id" =~ ^[0-9]+$ ]] || {
        echo "error: provide a v-prefixed tag and a numeric Actions run ID" >&2
        exit 1
    }
    GH_REPO="$(git remote get-url origin)"
    export GH_REPO
    git fetch origin "refs/tags/$tag:refs/tags/$tag"
    commit="$(git rev-parse "refs/tags/$tag^{commit}")"
    run="$(gh run view "$run_id" --json headSha,status,conclusion,workflowName,event --jq '[.headSha, .status, .conclusion, .workflowName, .event] | join("|")')"
    if [[ "$run" != "$commit|completed|success|Build|push" && "$run" != "$commit|completed|success|Build|workflow_dispatch" ]]; then
        echo "error: run must be a successful Build push or manual run of the tagged commit" >&2
        exit 1
    fi
    version="${tag#v}"
    artifacts="$(mktemp -d)"
    trap 'rm -rf -- "$artifacts"' EXIT
    gh run download "$run_id" --dir "$artifacts" \
        --name jog-linux-x86_64 \
        --name jog-windows-x86_64 \
        --name jog-ubuntu-24.04-amd64
    assets=(
        "$artifacts/jog-linux-x86_64/jog-$version-linux-x86_64"
        "$artifacts/jog-windows-x86_64/jog-$version-windows-x86_64.exe"
        "$artifacts/jog-ubuntu-24.04-amd64/jog_${version}_amd64.deb"
    )
    for asset in "${assets[@]}"; do
        [[ -s "$asset" ]] || {
            echo "error: missing or empty release asset: ${asset##*/}" >&2
            exit 1
        }
    done
    gh release create "$tag" "${assets[@]}" --verify-tag --generate-notes --title "$tag"
