#!/usr/bin/env bash
# Pre-release validation shared by release.yml (which calls this same
# script) and local pre-tag checks. Catches the tag-push-time failures
# before the tag ever leaves the machine.
# Usage: scripts/pre_release_check.sh v<version>
set -euo pipefail

tag="${1:?usage: scripts/pre_release_check.sh v<version>}"
version="${tag#v}"

cargo_version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
if [[ -z "$cargo_version" ]]; then
    echo "Failed to detect version from Cargo.toml" >&2
    exit 1
fi
if [[ "$cargo_version" != "$version" ]]; then
    echo "Tag $tag does not match Cargo.toml version $cargo_version" >&2
    exit 1
fi

# release.yml's notes extraction fails without this section; check both
# changelogs so the languages never drift apart at release time.
for changelog in CHANGELOG.md CHANGELOG.en.md; do
    if ! grep -qE "^## \[${version}\] - " "$changelog"; then
        echo "$changelog does not contain a '## [$version] - <date>' section" >&2
        exit 1
    fi
done

echo "pre-release checks ok: $tag == Cargo.toml $cargo_version, both changelogs carry the section"
