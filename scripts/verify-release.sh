#!/bin/sh
set -eu

archive=$1
staging=$(mktemp -d "${TMPDIR:-/tmp}/crash-monitor-verify.XXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM
tar -C "$staging" -xzf "$archive"

expected=$(sed -e '/^#/d' -e '/^$/d' packaging/production-binaries.txt | LC_ALL=C sort)
actual=$(find "$staging/bin" "$staging/libexec" -type f | sed "s|$staging/||" | LC_ALL=C sort)
test "$actual" = "$expected"
test -d "$staging/symbols/crash_monitor.dSYM"
test -d "$staging/symbols/crash_dialog_macos.dSYM"
(cd "$staging" && shasum -a 256 -c SHA256SUMS)
codesign --verify --strict "$staging/bin/crash_monitor"
codesign --verify --strict "$staging/libexec/crash_monitor/crash_dialog_macos"
codesign -d --entitlements :- "$staging/bin/crash_monitor" 2>&1 |
    grep -q 'com.apple.security.cs.debugger'
test "$(lipo -archs "$staging/bin/crash_monitor")" = "arm64"
