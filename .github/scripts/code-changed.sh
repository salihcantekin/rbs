#!/usr/bin/env bash
# Sets RBS_CODE=true in $GITHUB_ENV unless this is a pull request that changes only
# documentation, so the build and test steps can skip such pull requests.
#
# Documentation here means docs/, .claude/ and Markdown files at the repository root:
# no crate reads them (crate READMEs and doc comments live under crates/ and count as
# code). Pushes to main always build. The checkout must have fetch-depth 2: a pull
# request's checkout is its merge commit, whose first parent is the base branch.
set -euo pipefail

code=true
if [ "${GITHUB_EVENT_NAME:-}" = pull_request ]; then
  if files=$(git diff --name-only HEAD^1 HEAD); then
    if [ -n "$files" ] && ! grep -vqE '^(docs/|\.claude/|[^/]+\.md$)' <<< "$files"; then
      code=false
    fi
  else
    echo "could not list the changed files; building everything"
  fi
fi
echo "RBS_CODE=$code" >> "$GITHUB_ENV"
if [ "$code" = false ]; then
  echo "Documentation-only pull request: build and test steps are skipped." >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
fi
echo "RBS_CODE=$code"
