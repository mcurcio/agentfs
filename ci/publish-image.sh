#!/usr/bin/env bash
# Publishes a loaded image from image.yml's publish jobs: tags <image> as
# <repository>:<commit> and <repository>:latest, pushes both, and writes
# the digest to the step summary. Each push is retried: GHCR now and then
# answers a push with "unknown blob" that the next attempt completes
# (image run 39047451574), and pushing an image again is harmless.
#
# Usage: ci/publish-image.sh <image> <repository> <commit>
# PUBLISH_ATTEMPTS (default 3) and PUBLISH_RETRY_DELAY (seconds, default
# 15) set the retry.
set -euo pipefail

image=$1
repository=$2
commit=$3
attempts=${PUBLISH_ATTEMPTS:-3}
delay=${PUBLISH_RETRY_DELAY:-15}

for tag in "$commit" latest; do
  docker tag "$image" "$repository:$tag"
  attempt=1
  until docker push "$repository:$tag"; do
    if [ "$attempt" -ge "$attempts" ]; then
      echo "pushing $repository:$tag failed $attempts times" >&2
      exit 1
    fi
    echo "pushing $repository:$tag failed (attempt $attempt of $attempts); again in ${delay}s" >&2
    sleep "$delay"
    attempt=$((attempt + 1))
  done
done
digest=$(docker inspect --format '{{index .RepoDigests 0}}' "$repository:$commit")
{
  echo "### Published"
  echo
  echo "- \`$repository:$commit\`"
  echo "- \`$repository:latest\`"
  echo "- digest: \`$digest\`"
} >>"${GITHUB_STEP_SUMMARY:-/dev/stdout}"
