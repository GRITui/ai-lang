#!/usr/bin/env bash
# Every relative markdown link in the repo must resolve. Release notes that
# 404 the moment the tag is cut are a real defect: they are the first thing an
# installer user reads, and the release job publishes them verbatim.
#
# A broken link here would have caught a docs/STDLIB.md reference that was
# written before checking whether the file existed.
set -uo pipefail
cd "$(dirname "$0")/.."

broken=0
count=0
files=0

for md in README.md RELEASE_NOTES.md docs/*.md; do
  [ -f "$md" ] || continue
  files=$((files + 1))
  dir=$(dirname "$md")
  # Markdown links: [text](target). Skipped: absolute URLs, mailto, bare
  # anchors — only in-repo relative paths are ours to keep honest.
  while read -r link; do
    [ -n "$link" ] || continue
    case "$link" in
      http*|mailto:*|'#'*) continue ;;
    esac
    # Drop a #fragment and any ?query before resolving.
    target=${link%%#*}
    target=${target%%\?*}
    [ -n "$target" ] || continue
    count=$((count + 1))
    if [ -e "$dir/$target" ]; then
      echo "ok   $md -> $link"
    else
      echo "FAIL $md -> $link  (resolves to $dir/$target, which does not exist)"
      broken=$((broken + 1))
    fi
  done <<EOF
$(grep -oE '\]\(([^)]+)\)' "$md" 2>/dev/null | sed 's/^](//; s/)$//')
EOF
done

echo
if [ "$broken" -ne 0 ]; then
  echo "link check: $broken BROKEN link(s) across $files files"
  exit 1
fi
echo "link check: all $count relative links in $files files resolve"
