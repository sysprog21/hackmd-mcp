#!/usr/bin/env bash

# Delete and recreate rather than upload --clobber, so the "latest" tag moves to
# the commit that produced these binaries. That leaves a window where the
# release is gone and its replacement does not exist yet, which drives the care
# below: every input is checked before the first deletion.
#
# Expects GH_TOKEN, GH_REPO, GITHUB_SHA in the environment and the assets
# checked by stage-release-assets.sh in dist/.

set -eu

# Only 200 and 404 are answers. gh exits nonzero for every HTTP failure alike,
# so a 500 or a revoked token must abort here rather than read as "absent" and
# walk into the deletions below.
status_of()
{
    local status
    status=$(gh api "$1" --silent -i 2> /dev/null | head -1 | awk '{print $2}')
    case "$status" in
        200 | 404) echo "$status" ;;
        *)
            echo "unexpected status '${status:-none}' querying $1, aborting" >&2
            exit 1
            ;;
    esac
}

: "${GH_REPO:?}" "${GITHUB_SHA:?}" "${GH_TOKEN:?}"
shopt -s nullglob
assets=(dist/*)
if [ ${#assets[@]} -eq 0 ]; then
    echo "dist/ holds no assets" >&2
    exit 1
fi

# Re-running an older run on main would otherwise repoint "latest" backwards
# over newer binaries. Only the commit main points at now may publish.
head=$(gh api "repos/$GH_REPO/commits/main" --jq .sha)
if [ "$head" != "$GITHUB_SHA" ]; then
    echo "main is at $head, not $GITHUB_SHA; leaving the latest release alone"
    exit 0
fi

release_status=$(status_of "repos/$GH_REPO/releases/tags/latest")
if [ "$release_status" = 200 ]; then
    gh release delete latest --yes
fi

# Deleted explicitly rather than with --cleanup-tag, which fails when the tag is
# already gone. The tag can outlive its release (a web UI delete, a run that
# died after the delete above), and "gh release create" ignores --target when
# the tag exists, which would publish these binaries under an older commit.
tag_status=$(status_of "repos/$GH_REPO/git/ref/tags/latest")
if [ "$tag_status" = 200 ]; then
    gh api -X DELETE "repos/$GH_REPO/git/refs/tags/latest" > /dev/null
fi

# "gh release create" uploads into a draft and publishes it last, so a failed
# upload leaves a draft named "latest" behind, invisible to the lookups above.
# Captured first: a failed lookup inside the for list would not stop set -e.
drafts=$(gh api "repos/$GH_REPO/releases" --paginate \
    --jq '.[] | select(.draft and .tag_name == "latest") | .id')
for id in $drafts; do
    gh api -X DELETE "repos/$GH_REPO/releases/$id" > /dev/null
done

gh release create latest "${assets[@]}" \
    --target "$GITHUB_SHA" \
    --title "latest" \
    --notes "Automated build of ${GITHUB_SHA:0:7} on $(date -u +%Y-%m-%d). Check a download against SHA256SUMS, and its provenance as the README shows (gh attestation verify)."
