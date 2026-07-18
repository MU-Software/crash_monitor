# Privacy policy

Crash reports can contain application state and user data. The built-in policy
therefore treats stack bytes, the process memory map and heap summary,
environment variables, screenshots, registered attachments, and raw
shared-memory dumps as sensitive, opt-in evidence. Policy is normalized once
at startup; a legacy collector toggle cannot bypass the privacy gates, and the
same immutable decision controls collector registration, task-memory reads,
SHM copying, and Stage-1 persistence.

## Sensitive collection gates

Each sensitive evidence class is captured only when all three conditions are
true:

1. `privacy.level` permits its evidence class;
2. `privacy.consent` is `granted`;
3. its individual toggle is explicitly `true`.

| Privacy level | Stack bytes | Memory map / heap | Environment | Screenshots | Attachments | Raw SHM |
| --- | --- | --- | --- | --- | --- | --- |
| `minimal` (default) | off | off | off | off | off | off |
| `diagnostic` | consent + toggle | consent + toggle | off | off | off | off |
| `full` | consent + toggle | consent + toggle | consent + toggle | consent + toggle | consent + toggle | consent + toggle |

Every sensitive toggle defaults to `false`. Stack bytes use
`collectors.thread.stack_memory`; raw breadcrumb/context wire dumps use
`privacy.raw_shm`; the other four use their collector toggle. A full opt-in is
deliberate:

```json
{
  "privacy": {
    "level": "full",
    "consent": "granted",
    "encryption": "none",
    "raw_shm": true
  },
  "collectors": {
    "thread": { "enabled": true, "stack_memory": true },
    "memory": { "enabled": true },
    "environment": { "enabled": true },
    "screenshot": { "enabled": true },
    "attachment": { "enabled": true }
  }
}
```

`consent: "granted"` is a deployment-time assertion by the integrator. The
monitor does not display a consent prompt and this setting does not replace any
notice or consent flow required by the application or applicable law. Revoking
consent requires changing the config and restarting the monitor; it does not
retroactively erase already committed reports.

The `minimal` profile still contains process identifiers, thread registers and
backtraces, loaded-image metadata, and—when their ordinary collectors remain
enabled—formatted breadcrumbs and application-defined context. It does not
read or serialize stack bytes. Disable the breadcrumb, context, thread, or
image collector too, or use top-level `enabled: false`, when that baseline is
not acceptable.

The event snapshot copies only SHM sections required by an effectively enabled
collector or by the explicit raw opt-in. In particular, denied screenshot
pixels and attachment paths are not copied from the live mapping. When
`privacy.raw_shm` is authorized, Stage 1 additionally writes owned
`breadcrumbs.bin` and `context.bin`; these can be the only committed evidence
when later formatting fails, and ZIP archival can include them. They are
plaintext under the default `encryption: "none"` policy. Retention counts them
only after their transaction reaches the committed sent store; an incomplete
staging transaction remains subject to startup recovery rather than the sent
store's age scan.

## Compatibility

Regular JSON configuration files written for the former opt-out behavior still
parse, but their effective behavior is intentionally safer. For example,
`collectors.memory.enabled: true` without a `privacy` block is disabled during
normalization and produces a startup diagnostic. The legacy
`collectors.thread.enabled` shape remains valid, while its newly separated
`stack_memory` field defaults off. Migrating an installation that needs the
data requires adding the profile, consent, and individual opt-in fields; an
explicit `false` always remains off.

Only a genuinely missing `crash_reporter.json` selects defaults. An existing
unreadable or malformed file, a regular-file type mismatch, and both normal and
broken configuration symlinks fail startup before the child is spawned. This
prevents a requested encryption or consent policy from disappearing through a
parse/read fallback.

The privacy release also tightened retention defaults from 64 reports / 256
MiB / 15 days to 16 reports / 64 MiB / 7 days. A legacy partial retention
object inherits the new values for fields it omits and may therefore delete
older reports sooner. Pin every retention field explicitly before upgrading if
the former operational limits must be preserved.

## Retention

Automatic retention is enabled by default and bounds the sent-report store to
the first limit reached: 16 reports, 64 MiB, or 7 days. These are cleanup
bounds, not promises that every report will survive for seven days. They can be
tightened under `post_processors.retention`; disabling that post-processor or
raising its limits is an explicit operator decision.

Retention deletes whole committed report transactions after notification. It
is best-effort deletion, not cryptographic erasure: filesystem snapshots,
backups, copied ZIP files, and storage remanence may retain data independently.
Its quota and age scan covers committed reports in the sent store only. Hidden
or incomplete `.report-*.pending` transactions are outside these retention
limits and require the separate startup recovery/scavenger lifecycle; operators
must not treat the sent-store age limit as a bound on abandoned staging data.

## Encryption

The current artifact format has **no application-layer encryption**.
`privacy.encryption: "none"` (the default) records that fact. Private file
permissions restrict local access but are not encryption; deployments that
need encryption at rest must place the data directory on an externally managed
encrypted volume and account for backups and exported artifacts separately.

Setting `privacy.encryption: "required"` is a fail-closed assertion. This build
rejects the configuration before capture because it cannot satisfy the
requirement, rather than silently writing plaintext. The top-level
`enabled: false` kill switch is the sole exception because it creates no report
artifacts at all.
