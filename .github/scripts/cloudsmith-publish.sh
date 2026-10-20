#!/bin/sh
# Pushes the packages that the Publish workflow built to Cloudsmith, and
# writes a table of what happened to $GITHUB_STEP_SUMMARY.
#
# usage: cloudsmith-publish.sh <artifacts-dir>
#
# Every subdirectory of <artifacts-dir> is one target's artifact: its package
# and target.json, the target's entry of the build matrix. With DRY_RUN=true
# nothing is checked or pushed; every package is reported as dry-run.
#
# A package already in the repository is skipped, not pushed again, so that a
# rerun on the same tag completes what an earlier one left out and never
# replaces a published file. Needs the cloudsmith CLI, authenticated, and jq.

set -eu

artifacts=$1
owner=${CLOUDSMITH_OWNER:-m-compressors}
dry_run=${DRY_RUN:-false}
summary=${GITHUB_STEP_SUMMARY:-/dev/stdout}
rows=$(mktemp)
failed=false

# Distribution slugs as Cloudsmith knows them, for example debian/trixie.
distros() {
    cloudsmith list distros "$1" -F json |
        jq -r '.data[] | .slug as $d | .versions[] | "\($d)/\(.slug)"'
}
if [ "$dry_run" != true ]; then
    known=$(mktemp)
    distros deb >"$known"
    distros rpm >>"$known"
fi

for dir in "$artifacts"/*/; do
    target=$dir/target.json
    id=$(jq -r .id "$target")
    repo=$(jq -r .repository "$target")
    distro=$(jq -r '"\(.distro)/\(.release)"' "$target")
    format=$(jq -r .family "$target")
    package=$(find "$dir" -maxdepth 1 -name "*.$format" | head -1)
    [ -n "$package" ] || {
        echo "::error::no .$format package for $id"
        exit 1
    }
    file=$(basename "$package")

    if [ "$dry_run" = true ]; then
        status=dry-run
    elif ! grep -qx "$distro" "$known"; then
        echo "::error::Cloudsmith has no $format distribution $distro (for $id)"
        status=failed
        failed=true
    else
        # Search on the name up to its first `+` or `~`, which the query
        # syntax may read as operators, and compare the whole name here.
        prefix=$(echo "$file" | sed 's/[+~].*//')
        existing=$(cloudsmith list packages "$owner/$repo" -q "filename:$prefix" --page-all -F json |
            jq --arg file "$file" '[.data[] | select(.filename == $file)] | length')
        if [ "$existing" -gt 0 ]; then
            echo "$file is already in $repo, skipping"
            status=skipped
        elif cloudsmith push "$format" "$owner/$repo/$distro" "$package"; then
            status=published
        else
            echo "::error::pushing $file to $repo/$distro failed"
            status=failed
            failed=true
        fi
    fi
    printf "| %s | %s | %s | \`%s\` | %s |\n" "$id" "$repo" "$distro" "$file" "$status" >>"$rows"
done

{
    echo "### Cloudsmith"
    echo
    echo "| Target | Repository | Distribution | File | Status |"
    echo "|---|---|---|---|---|"
    sort "$rows"
} >>"$summary"

[ "$failed" = false ]
