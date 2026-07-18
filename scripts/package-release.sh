#!/bin/sh
set -eu

output_dir=$1
monitor=$2
dialog=$3
mkdir -p "$output_dir"
archive="$output_dir/crash-monitor.tar.gz"
staging=$(mktemp -d "$output_dir/.staging.XXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM

mkdir -p "$staging/bin" "$staging/libexec/crash_monitor" "$staging/symbols"
cp "$monitor" "$staging/bin/crash_monitor"
cp "$dialog" "$staging/libexec/crash_monitor/crash_dialog_macos"
dsymutil "$monitor" -o "$staging/symbols/crash_monitor.dSYM"
dsymutil "$dialog" -o "$staging/symbols/crash_dialog_macos.dSYM"
cp packaging/production-binaries.txt "$staging/production-binaries.txt"
(
    cd "$staging"
    find bin libexec symbols -type f -print | LC_ALL=C sort | xargs shasum -a 256 > SHA256SUMS
)
tar -C "$staging" -czf "$archive" .
printf '%s\n' "$archive"
