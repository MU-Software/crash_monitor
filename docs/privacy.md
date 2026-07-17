# Privacy policy

Crash reports can contain application state and user data. The built-in policy
therefore treats environment variables, the process memory map and heap
summary, screenshots, and registered attachments as sensitive, opt-in evidence.
Policy is normalized once at startup; a legacy collector toggle cannot bypass
the privacy gates.

## Sensitive collection gates

Each sensitive collector runs only when all three conditions are true:

1. `privacy.level` permits its evidence class;
2. `privacy.consent` is `granted`;
3. its `collectors.<name>.enabled` toggle is explicitly `true`.

| Privacy level | Memory map / heap summary | Environment | Screenshots | Attachments |
| --- | --- | --- | --- | --- |
| `minimal` (default) | off | off | off | off |
| `diagnostic` | allowed with consent + toggle | off | off | off |
| `full` | allowed with consent + toggle | allowed with consent + toggle | allowed with consent + toggle | allowed with consent + toggle |

The four collector toggles also default to `false`. A full opt-in is deliberate:

```json
{
  "privacy": {
    "level": "full",
    "consent": "granted",
    "encryption": "none"
  },
  "collectors": {
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

The `minimal` profile is scoped to the four evidence classes above. A normal
crash report can still contain process identifiers, thread registers,
backtraces and bounded stack bytes, loaded-image metadata, breadcrumbs, and
application-defined context. Disable those collector toggles as well, or use
top-level `enabled: false`, when that evidence is not acceptable.

There is an additional current raw-capture boundary: when shared memory is
available, Stage 1 writes owned `breadcrumbs.bin` and `context.bin` snapshots
even if the formatted Breadcrumb and Context collector toggles are disabled.
Those raw files can be the only committed evidence when later finalization
fails, and ZIP archival can include them. The configured encryption policy
applies to them (so they are plaintext under the default `none` policy).
Retention counts them only when their transaction reaches the committed sent
store; a raw-only or staging transaction outside that scan has no sent-store
age guarantee. The `minimal` profile does not suppress them. Use top-level
`enabled: false` when capturing application-provided SHM data is unacceptable.

## Compatibility

Configuration files written for the former opt-out behavior still parse, but
their effective behavior is intentionally safer. For example,
`collectors.memory.enabled: true` without a `privacy` block is disabled during
normalization and produces a startup diagnostic. Migrating an installation
that genuinely needs the data requires adding the profile and consent fields;
an individual collector explicitly set to `false` always remains off.

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
