#!/bin/sh
set -eu

query=${1-}
if [ -z "$query" ]; then
    echo "SIGN_IDENTITY is required for privileged signing" >&2
    exit 2
fi

identities=$(security find-identity -v -p codesigning)
matches=$(
    printf '%s\n' "$identities" |
        SIGN_IDENTITY_QUERY="$query" awk '
            length($2) == 40 && $2 ~ /^[[:xdigit:]]+$/ && index($0, ENVIRON["SIGN_IDENTITY_QUERY"]) {
                name = $0
                sub(/^[^"]*"/, "", name)
                sub(/"[^"]*$/, "", name)
                print $2 "\t" name
            }
        '
)

match_count=$(printf '%s\n' "$matches" | awk 'NF { count++ } END { print count + 0 }')
if [ "$match_count" -eq 0 ]; then
    echo "codesigning identity not found: $query" >&2
    exit 2
fi

if [ "$match_count" -gt 1 ]; then
    name_count=$(printf '%s\n' "$matches" | awk -F '\t' '!seen[$2]++ { count++ } END { print count + 0 }')
    if [ "$name_count" -gt 1 ]; then
        echo "codesigning identity is ambiguous: $query" >&2
        printf '%s\n' "$matches" | awk -F '\t' '{ print "  " $1 " \"" $2 "\"" }' >&2
        echo "set SIGN_IDENTITY to an exact identity name or SHA-1" >&2
        exit 2
    fi
fi

resolved=$(printf '%s\n' "$matches" | awk 'NR == 1 { print $1 }')
if [ "$match_count" -gt 1 ]; then
    echo "multiple certificates share identity '$query'; using SHA-1 $resolved" >&2
fi
printf '%s\n' "$resolved"
