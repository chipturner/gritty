#!/usr/bin/env bash
set -euo pipefail

# Publish recorded demo GIFs to the gh-pages branch (under demo/) so the
# README can hot-link them without committing binaries to main.
#
## One-time setup (run once, by a maintainer)
#
# gritty has no `gh-pages` branch yet. Create an orphan with a single
# placeholder commit so this script can fetch it:
#
#   git worktree add --detach .worktrees/gh-pages-init
#   cd .worktrees/gh-pages-init
#   git checkout --orphan gh-pages
#   git rm -rf --quiet . && echo "demo GIFs for the README (published by demo/publish.sh)" > README.md
#   git add README.md && git commit -m "gh-pages: init"
#   git push origin gh-pages
#   cd ../.. && git worktree remove .worktrees/gh-pages-init
#
# Then in the GitHub repo settings, Pages -> Source: `gh-pages` branch,
# `/ (root)` (or `gh settings` equivalent). The GIFs will be served at
# https://chipturner.github.io/gritty/demo/<name>.gif
#
# Finally, publish the current GIFs so the README never 404s. docs/images/
# was removed from this repo, so pull the last blessed copies out of history
# (commit 57afe8c is the last one that has them), or re-record with `just
# demo`. demo/out/ is gitignored -- it's a staging area, not a tracked dir:
#
#   mkdir -p demo/out
#   git show 57afe8c:docs/images/persist.gif > demo/out/persist.gif
#   git show 57afe8c:docs/images/transfer.gif > demo/out/transfer.gif
#   just demo-publish
#   curl -sSfI https://chipturner.github.io/gritty/demo/persist.gif | head -1
#
# Expected: `HTTP/2 200` (Pages deploys take a minute; retry).

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
WORKTREE="$REPO_ROOT/.worktrees/gh-pages"

die() { echo "error: $*" >&2; exit 1; }

gifs=("$SCRIPT_DIR"/out/*.gif)
[[ -f "${gifs[0]}" ]] || die "no GIFs in demo/out/; run demo/record.sh first"

git -C "$REPO_ROOT" fetch origin gh-pages
if [[ ! -d "$WORKTREE" ]]; then
    mkdir -p "$(dirname "$WORKTREE")"
    git -C "$REPO_ROOT" worktree add "$WORKTREE" gh-pages
fi
git -C "$WORKTREE" pull --ff-only origin gh-pages

mkdir -p "$WORKTREE/demo"
cp "${gifs[@]}" "$WORKTREE/demo/"
git -C "$WORKTREE" add demo
if git -C "$WORKTREE" diff --cached --quiet; then
    echo "gh-pages already up to date"
    exit 0
fi
git -C "$WORKTREE" commit -m "demo: update recorded GIFs"
git -C "$WORKTREE" push origin gh-pages
echo "published: https://chipturner.github.io/gritty/demo/"
