#!/usr/bin/env bash
#
# Safe self-merge (task 718): wrap `gh pr merge` with a hard pre-merge assertion that the PR is
# NOT empty, so a stale-branch push can never self-merge a no-op PR that still auto-deploys from
# main HEAD. (That happened once -- an empty PR merged + deployed, caught only by hand-grepping
# origin/main and recovered via reflog.) GitOps deploys main HEAD directly and there is no CI, so
# the merge step itself is the only place to catch it: always merge through this script.
#
#     nix develop -c scripts/self-merge.sh [<pr-number>]   # defaults to the current branch's PR
#
# Fail-closed: a 0-file diff aborts the merge (exit 1). Also warns (does not block) if the PR head
# commit differs from the local HEAD you gated, so "I gated X but the PR carries Y" is visible.
set -euo pipefail

pr="${1:-}"
# Build the `gh pr view` selector: an explicit number, else the current branch's PR.
if [ -n "$pr" ]; then
  sel=("$pr")
else
  sel=()
fi

view_json() { gh pr view "${sel[@]}" --json "$1" -q "$2"; }

number=$(view_json number '.number')
state=$(view_json state '.state')
if [ "$state" != "OPEN" ]; then
  echo "ABORT: PR #$number is $state, not OPEN -- nothing to merge." >&2
  exit 1
fi

files=$(view_json files '.files | length')
if [ "$files" -eq 0 ]; then
  echo "ABORT: PR #$number has an EMPTY diff (0 changed files)." >&2
  echo "  Refusing to merge a no-op PR -- it would auto-deploy from main HEAD for nothing." >&2
  echo "  Likely a stale-branch push: confirm your gated commit is actually on the pushed branch" >&2
  echo "  (git log origin/\$(git branch --show-current)) and re-push, then retry." >&2
  exit 1
fi

# Soft parity check: the PR head should be the commit you gated locally.
pr_head=$(view_json headRefOid '.headRefOid')
local_head=$(git rev-parse HEAD 2>/dev/null || echo "")
if [ -n "$local_head" ] && [ "$pr_head" != "$local_head" ]; then
  echo "WARNING: PR #$number head ($pr_head) != local HEAD ($local_head)." >&2
  echo "  What you gated may differ from what will merge -- verify before relying on this merge." >&2
fi

echo "self-merge: PR #$number has $files changed file(s); merging (squash + delete branch)."
gh pr merge "$number" --squash --delete-branch
