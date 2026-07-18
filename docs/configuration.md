# Configuration reference

Crash Monitor reads one JSON object from
`<data-dir>/crash_reporter.json`. Run `crash_monitor check-config` before a
deployment; it loads the same file, applies the same range and dependency
validation as `run`, prints policy diagnostics, and exits nonzero on error.

A missing file selects the defaults below. An existing file that is unreadable,
not a regular file, a symlink, malformed JSON, contains an unknown field, or
violates a range stops startup before the child is launched. Configuration is
loaded and validated once; the event loop and pipeline share that immutable
snapshot.

## Complete default document

This is an executable description of all fields and defaults. Fields may be
omitted because nested objects use these defaults.

```json
{
  "enabled": true,
  "report_dir": null,
  "privacy": {
    "level": "minimal",
    "consent": "not_granted",
    "encryption": "none",
    "raw_shm": false
  },
  "triggers": {
    "enabled": true,
    "crash": { "enabled": true },
    "exit_failure": { "enabled": true },
    "signal_failure": { "enabled": true },
    "oom_detection": { "enabled": false },
    "anr": { "enabled": true },
    "snapshot": { "enabled": true }
  },
  "watchdog": {
    "warmup_ms": 10000,
    "threshold_ms": 5000,
    "check_interval_ms": 2000,
    "cooldown_ms": 60000
  },
  "filters": {
    "enabled": true,
    "disk_space": { "enabled": true, "min_free_mb": 100 },
    "rate_limiter": { "enabled": true, "max_events": 3, "window_secs": 60 }
  },
  "collectors": {
    "enabled": true,
    "thread": { "enabled": true, "stack_memory": false },
    "breadcrumb": { "enabled": true },
    "context": { "enabled": true },
    "memory": { "enabled": false },
    "dylib": { "enabled": true },
    "screenshot": { "enabled": false },
    "attachment": { "enabled": false },
    "environment": { "enabled": false },
    "process_output": { "enabled": false }
  },
  "pre_processors": {
    "enabled": true,
    "session": { "enabled": true },
    "symbolizer": { "enabled": true },
    "fingerprint": { "enabled": true, "top_frames": 8 },
    "build_info": { "enabled": true },
    "duplicate": { "enabled": true, "window_secs": 60 },
    "sanitizer": { "enabled": true }
  },
  "post_processors": {
    "enabled": true,
    "raw_cleanup": { "enabled": true },
    "session_recorder": { "enabled": true },
    "png_converter": { "enabled": true },
    "feedback_dialog": { "enabled": true },
    "zip_archiver": { "enabled": true },
    "move_to_sent": { "enabled": true },
    "log_rotator": { "enabled": true, "max_size_mb": 1 },
    "retention": {
      "enabled": true,
      "max_reports": 16,
      "max_size_mb": 64,
      "max_age_days": 7
    }
  },
  "notifiers": {
    "enabled": true,
    "console": { "enabled": true },
    "system_notification": { "enabled": false }
  }
}
```

`report_dir`, when present, must be an absolute path. The normal default is
`<data-dir>/crashes/pending`; see [integration.md](integration.md#data-directory).

## Enablement and privacy

Top-level `enabled=false` is the global report-generation kill switch. The
monitor continues child supervision, Mach replies, and reaping, but performs no
capture, SHM evidence read, plugin invocation, or artifact write. There is no
emergency-evidence exception. A category's `enabled=false` disables every
plugin or trigger below that category; an individual toggle can only narrow it.

Trigger meanings are independent:

- `crash`: fatal subscribed Mach exception;
- `exit_failure`: primary nonzero exit without an earlier Mach crash;
- `signal_failure`: primary signal death not classified as OOM;
- `oom_detection`: opt-in classification of primary SIGKILL as
  `possible_oom`, never proof of OOM;
- `anr`: producer-ready heartbeat stall;
- `snapshot`: SIGUSR1 manual snapshot.

Sensitive collectors require all three gates: the collector toggle, a
sufficient `privacy.level`, and `privacy.consent="granted"`. `diagnostic`
permits stack bytes and memory diagnostics; `full` additionally permits
environment, process output, screenshots, attachments, and `raw_shm` when each
specific switch is enabled. `encryption="required"` currently fails closed
because application-layer encryption is not implemented.

## Ranges and zero values

All sizes and durations are unsigned integers. Numeric fields that represent a
duration, sampling interval, frame count, or rotation threshold must be greater
than zero. `watchdog.check_interval_ms` must not exceed
`watchdog.threshold_ms`. `retention.max_reports` must be greater than zero while
retention is enabled. `retention.max_size_mb=0` or `max_age_days=0` is an
explicit immediate-removal policy, not “unlimited.”

`rate_limiter.max_events=0` explicitly rejects every event in its window; turn
the filter off with `rate_limiter.enabled=false` instead. Likewise, use an
`enabled` switch rather than a numeric sentinel to disable a plugin.

## Dependency and ordering policy

Plugin IDs are stable and globally unique. A hard data dependency being
disabled transitively disables its dependent and emits a diagnostic; it is not
silently re-enabled. An order-only provider may be disabled without disabling
the later plugin. Duplicate IDs, missing dependencies, cross-category edges,
cycles, and invalid registration order are fatal validation errors.

The built-in hard edge is `DuplicateDetector → Fingerprinter`. Important
order-only edges are `Fingerprinter → SymbolResolver`, `FeedbackDialog →
PNGConverter`, `ZIPArchiver → RawCleanup/PNGConverter/FeedbackDialog`,
`MoveToSent → ZIPArchiver`, and `RetentionManager → ZIPArchiver/MoveToSent`.
Arrows here mean “left runs after right.”

## Operations overrides

JSON is the primary source for watchdog timing. The four legacy
`CRASH_MONITOR_ANR_*` variables are ignored unless the explicit operations/test
override gate is enabled. When enabled, each present variable replaces only its
matching validated JSON value and the resulting set is range-checked again.
Production launch definitions should prefer the checked JSON file so effective
policy is reviewable and reproducible.
