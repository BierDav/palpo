#!/usr/bin/env bash
set -euo pipefail

# Refresh the fork before reading its base; a scheduled run may have queued.
git fetch origin patched
git checkout --detach origin/patched
current_revision="$(tr -d '[:space:]' < .custom-upstream-revision)"
git fetch --no-tags https://github.com/palpo-im/palpo.git main
latest_revision="$(git rev-parse FETCH_HEAD)"
echo "Current upstream base: ${current_revision}; upstream main: ${latest_revision}"

if [[ "${latest_revision}" == "${current_revision}" ]]; then
  echo "Already up to date."
  exit 0
fi

# The release tag is version metadata; the fork only publishes custom tags.
latest_tag="$(gh api repos/palpo-im/palpo/releases/latest --jq '.tag_name')"
git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"

# Preserve the complete fork history. Path-filtered cherry-picks still replay
# whole commits, including mixed CI/source changes, and repeat obsolete patches.
if ! git merge --no-ff --no-commit "${latest_revision}"; then
  status="$(git status --short)"
  git merge --abort
  {
    echo "## Upstream sync conflict"
    echo "Merging upstream ${latest_revision} into the custom fork failed."
    echo '```'
    echo "${status}"
    echo '```'
  } >> "$GITHUB_STEP_SUMMARY"
  exit 1
fi

printf '%s\n' "${latest_tag}" > .custom-upstream-tag
printf '%s\n' "${latest_revision}" > .custom-upstream-revision
git add .custom-upstream-tag .custom-upstream-revision
git commit -m "ci: merge upstream ${latest_revision} into custom build"

base_ver="${latest_tag#v}"
max_n=0
while IFS= read -r tag; do
  n="${tag##*-custom.}"
  if [[ "$n" =~ ^[0-9]+$ ]] && (( n > max_n )); then
    max_n="$n"
  fi
done < <(git tag -l "${base_ver}-custom.*")
release_tag="${base_ver}-custom.$((max_n + 1))"

# A concurrent fork update must reject this push, never overwrite that update.
git push origin HEAD:patched
gh workflow run -R "${GITHUB_REPOSITORY}" custom-release.yml --ref patched -f "release_tag=${release_tag}"
