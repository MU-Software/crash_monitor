# Operations troubleshooting

Start with the monitor's first fatal diagnostic and preserve the data directory
before attempting cleanup. Commands below are read-only unless explicitly
labelled as recovery.

## Entitlement and `task_for_pid` failures

Verify the binary being launched, its signature, and the boolean entitlement:

```bash
command -v crash_monitor
codesign --verify --deep --strict --verbose=4 /absolute/path/crash_monitor
codesign -d --entitlements :- /absolute/path/crash_monitor
```

The entitlement output must contain
`com.apple.security.cs.debugger = true`; a missing key, string value, or boolean
false is not sufficient. Confirm that launch tooling did not substitute an
unsigned debug binary after signing. Build without signing with
`make build-unsigned`, then use the explicit signing target and identity for a
privileged run. If the signature is valid but `task_for_pid` remains denied,
check the active account/session policy and system security logs; do not run the
monitor as root as a blanket workaround.

## Helper signature and architecture mismatch

The dialog/helper must be the signed sibling expected by the package, not a
PATH-resolved replacement:

```bash
codesign --verify --deep --strict --verbose=4 /install/dir/crash_dialog_macos
codesign -dvv /install/dir/crash_monitor
codesign -dvv /install/dir/crash_dialog_macos
file /install/dir/crash_monitor /install/dir/crash_dialog_macos /path/to/target-app
lipo -archs /install/dir/crash_monitor /install/dir/crash_dialog_macos
uname -m
```

The supported deployment is native arm64 macOS. Both packaged binaries must
match the release manifest's Team ID/signature requirements; x86_64/Rosetta is
not a supported fallback. A mock dialog belongs only to test builds and must
never be copied into a production package.

## Orphan child or process group

Identify the exact monitor PID, child PID, and process group before signaling:

```bash
ps -axo pid,ppid,pgid,state,lstart,command | grep -E 'crash_monitor|target-app'
lsof -p MONITOR_PID
```

Send `SIGTERM` to the monitor first and wait for its bounded shutdown. If the
monitor is gone but its dedicated child process group remains, send `SIGTERM`
to that exact positive PGID using `kill -TERM -- -PGID`, re-run `ps`, then use
`SIGKILL` only for the same verified PGID after the grace period. Never use a
zero, negative variable, wildcard, or guessed PID/PGID. Preserve logs and the
report directory because orphaning indicates a supervisor cleanup failure.

## Stale shared memory

Each SHM name contains a monitor PID and random nonce and is unlinked by its
owner during normal teardown. Use `lsof -p MONITOR_PID` while the owner is live
to confirm open shared-memory descriptors. POSIX SHM objects are kernel-managed
names on macOS; do not search for and delete similarly named ordinary files.
If a dead monitor demonstrably left a name, first ensure no process has it open,
then use a purpose-built `shm_unlink` diagnostic matching that exact recorded
name or reboot the affected test host. Record the name and failure before
cleanup so the ownership bug remains diagnosable.

## Incomplete `pending` transaction

Hidden `.report-<id>.pending` directories are not committed reports. Check
ownership/mode and whether an owner still has them open:

```bash
ls -ldeO /data/root/crashes/pending /data/root/crashes/pending/.report-*.pending
lsof +D /data/root/crashes/pending
```

Restarting the same build is the preferred recovery: it validates a fully
synced manifest and exact artifact set before atomic publication, skips a live
owner lock, and scavenges abandoned incomplete staging only after its age
threshold. Do not rename a hidden directory into visibility by hand and do not
edit its manifest to force recovery. Quarantine a persistently rejected
directory outside the active data root for later inspection.

## Failed or missing archive

Read `manifest.json` first. A valid committed report may intentionally name
`report.json` when ZIP creation was disabled or failed before commit. If it
names `report.zip`, verify that exact regular file and size; do not choose a
similarly prefixed file. Check free space, `0700`/`0600` modes, ownership, ACLs,
and the `ZIPArchiver` diagnostic. Never replace an artifact inside a committed
transaction because the manifest size and atomicity contract would be false;
make a private review copy and create a separate archive instead.

## Disable collection and clean sensitive reports

Set top-level `"enabled": false`, run `crash_monitor check-config`, and restart
the monitor. This keeps child supervision but disables capture, plugins, SHM
evidence reads, and artifact writes. Confirm no new ReportId appears before
handling existing data.

For deletion, stop every monitor using the root, make any legally or
operationally required preservation copy, and resolve the exact absolute
`CRASH_MONITOR_DATA_DIR`. Delete only selected committed ReportId directories
through an approved file-management workflow or rely on tightened retention;
do not issue a recursive command against `$HOME`, `/`, a workspace root, or an
unverified variable. Deletion is best effort, not cryptographic erasure, and
does not remove backups, exported archives, or recipient copies.
