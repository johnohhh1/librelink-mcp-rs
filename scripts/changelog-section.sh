#!/bin/sh
# Prints the body of one version's section of CHANGELOG.md: the lines after
# "## <version> " up to the next "## " heading. Fails if there is none.
# Usage: scripts/changelog-section.sh 1.0.0 [CHANGELOG.md]
set -eu
version="$1"
file="${2:-CHANGELOG.md}"
section=$(awk -v heading="## $version " '
    index($0, heading) == 1 { found = 1; next }
    found && /^## / { exit }
    found { print }
' "$file")
if [ -z "$(printf '%s' "$section" | tr -d '[:space:]')" ]; then
    echo "No CHANGELOG.md section for $version." >&2
    exit 1
fi
printf '%s\n' "$section"
