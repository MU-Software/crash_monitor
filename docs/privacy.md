# Privacy policy

Crash reports can contain application state and user data. The built-in policy
therefore treats stack bytes, the process memory map and heap summary,
environment variables, child stdout/stderr tails, screenshots, registered attachments, and raw
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

| Privacy level | Stack bytes | Memory map / heap | Environment | Process output | Screenshots | Attachments | Raw SHM |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `minimal` (default) | off | off | off | off | off | off | off |
| `diagnostic` | consent + toggle | consent + toggle | off | off | off | off | off |
| `full` | consent + toggle | consent + toggle | consent + toggle | consent + toggle | consent + toggle | consent + toggle | consent + toggle |

Every sensitive toggle defaults to `false`. Stack bytes use
`collectors.thread.stack_memory`; raw breadcrumb/context wire dumps use
`privacy.raw_shm`; the other five use their collector toggle. A full opt-in is
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
    "process_output": { "enabled": true },
    "screenshot": { "enabled": true },
    "attachment": { "enabled": true }
  }
}
```

### Evidence classification

| Evidence | Sensitivity and handling |
|---|---|
| Environment | highly sensitive; even the allowlist may reveal locale, terminal, and timezone |
| Hostname | identifying; the built-in collector emits `[REDACTED]` rather than the host value |
| Thread registers/backtraces | diagnostic addresses and paths; treat as sensitive |
| Stack bytes / memory map / heap | highly sensitive; bytes may contain arbitrary user data or secrets |
| Screenshot | highly sensitive pixels with no general redaction; full-profile opt-in only |
| Breadcrumb / annotation | producer-defined activity and state text; potentially sensitive even in the minimal profile |
| Attachment label and source path | identifying metadata; sanitized in JSON but still sensitive |
| Attachment content | arbitrary user-controlled bytes; highly sensitive and not content-redacted |
| Child stdout/stderr | arbitrary application text; highly sensitive |
| Raw SHM | opaque producer bytes; highly sensitive and not content-redacted |

Environment capture uses a minimal allowlist (`LANG`, `LC_*`, `TERM`, and
`TZ`) rather than attempting to enumerate secret names. URL/DSN/cookie and
credential/key variables are therefore excluded by default. Hostname is
serialized as `[REDACTED]`. Text that reaches a report—including thread names,
breadcrumbs, annotations, attachment labels/original paths, child-output
tails, and feedback—is sanitized again immediately before each JSON write.
Exact home directories, descendants, and username-only values are masked;
macOS path matching is ASCII case-insensitive to match common volume behavior.
Child output has no reliable secret allowlist, so its bounded tails require the
full profile, granted consent, and the explicit `collectors.process_output`
toggle even though the monitor continues draining the pipes to avoid child
backpressure.

Screenshot pixels have no reliable general-purpose redaction transform. They
remain excluded by default and require the full privacy profile, deployment
consent, and the individual screenshot toggle shown above. Integrators must
surface that explicit choice in their own consent UI. Raw SHM wire dumps are
likewise excluded by default and require the separate `privacy.raw_shm` opt-in;
unlike decoded textual report fields, their opaque binary payload is not
content-redacted.

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

Registered attachments have an additional filesystem boundary. The production
copier pins the monitor's startup working directory as a trusted directory
descriptor and accepts only normal path components below it. Labels and
extensions are reduced to bounded ASCII filename components. Every parent is
opened relative to the preceding descriptor with
`openat(O_DIRECTORY | O_NOFOLLOW)` and verified by `fstat`; the final source is
opened with `O_NOFOLLOW | O_NONBLOCK` and must be regular. Bytes are streamed
from that already-open descriptor through
the 50 MiB cap and cooperative deadline; the path is never reopened for copy.
Destinations use private exclusive temporary files, random UUID names, and
no-clobber atomic publication.

Attachment collection remains behind the `full` profile, granted consent, and
explicit attachment toggle whether reports stay local or a future uploader is
installed. An uploader must treat the committed manifest as its allowlist and
must not reread producer source paths. Adding transport does not widen consent;
deployments must separately disclose remote transfer and obtain any required
upload consent before enabling it.

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

## Local storage boundary

Crash Monitor creates every managed data, staging, pending, sent, and report
directory with mode `0700`. Report JSON, raw SHM dumps, RGBA/PNG attachments,
ZIP archives, manifests, session/log files, and their temporary files use mode
`0600`. Descriptor-based mode correction makes those results independent of
the process umask. Existing managed nodes must be owned by the effective user;
unsafe types and extended ACLs are rejected, while owned POSIX mode drift is
corrected to the exact private mode.

Path traversal opens each component relative to an already validated directory
descriptor with no-follow semantics. Symlinks, untrusted writable ancestors,
and ACLs that grant additional access fail closed. New files use exclusive
creation, and final artifact publication uses an exclusive atomic rename so a
concurrent or attacker-created destination is never replaced. The source inode
and its private permissions are checked again after rename before publication
reports success. Report-directory commit and recovery also revalidate the
manifest bytes, exact artifact set, and recorded sizes immediately before that
rename. A report becomes visible only as a complete manifest-validated
transaction. Directory-sync failures after that publication boundary are
surfaced as durability warnings; they do not relabel the already visible report
as unpublished.

These filesystem controls isolate other accounts, not arbitrary code already
running as the same effective UID. Such a process is inside the same local
trust boundary and can alter user-owned files after publication; deployments
that require stronger isolation must run the monitor under a dedicated account
or an equivalent sandbox.

User-selected CLI export locations are not converted into managed `0700`
directories. Their existing safe parent mode is preserved, but the parent is
still validated against symlink, untrusted-write, and allowing-ACL attacks, and
the newly exported file is `0600`.

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

## Review and sharing procedure

Before sending a report to support, a vendor, or any third party:

1. Copy the committed `<ReportId>` directory to a private review location; do
   not share a hidden `.report-*.pending` staging directory.
2. Read `manifest.json` and review every listed artifact. Use `crash_monitor
   analyze report.zip` for the summary, then inspect the ZIP entries and JSON.
3. Remove or redact environment values, stack bytes, memory data, screenshots,
   breadcrumbs, annotations, feedback, attachment metadata/content, child
   output, usernames, home paths, and identifiers that are not necessary for
   the recipient. Rebuild a new archive rather than editing the committed
   monitor transaction in place.
4. Re-open the new archive and verify its exact contents, destination, and
   recipient. Transfer it only through an approved encrypted channel with an
   explicit retention/deletion agreement.
5. Delete the review copy when the support purpose ends, recognizing that
   backups, recipient copies, and storage remanence are outside monitor
   retention.

Stage-1 raw files are fail-safe capture evidence and may be less redacted than
the final JSON. The default final `report.zip` can contain those raw files plus
JSON, screenshots, and attachments; “archived” does not mean “sanitized” or
“encrypted.” Always review the manifest and archive entries, not only the
human-readable summary.

Crash Monitor does not currently upload reports. Adding an uploader requires a
separate, disclosed remote-transfer consent; local collection consent does not
authorize network sharing. An uploader must use the committed manifest as its
only allowlist, avoid hidden/incomplete transactions, authenticate the
destination, encrypt transport, expose retry/deletion behavior, and preserve
the user's ability to revoke future uploads.
