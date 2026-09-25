# Session findings — 2026-09-25

The latest locally retained session ran **v0.3.4**, from 03:04 to 05:02 UTC.
Startup already logs the compiled package version. Both this session and its
immediately preceding v0.3.4 session completed cleanly. This note omits private
player identifiers, endpoints, sequence numbers, and packet payloads.

## Dungeon mob phases

Issue #476 tracks idle-timeout endings followed by `NewFight` resets between
mob waves. The old idle policy deliberately exempted engaged recognized bosses,
but let ordinary dungeon packs time out after nine seconds. The requested
behavior is to retain the run's totals across mob phases leading to the boss.
Merged PR #478 suppresses this idle split while a known dungeon scene has
a nonterminal dungeon state. Explicit ends still take precedence; sessions
without run-state evidence retain the prior idle fallback. Synthetic regressions
exercise the policy change; raw captures remain local.

## Existing issues and evidence limits

- **#454 remains reproducible.** The latest run recovered three TCP holes:
  20 bytes after 19,485 ms at 03:44:23 UTC; 421 bytes after 11,815 ms at
  04:57:36; and 410 bytes after 10,067 ms at 04:57:46. Processing resumed,
  including a later boss-death/history record, but this does not establish
  completeness. The issue has updated evidence. Decoded dumps cannot determine
  why transport bytes were missing; a matching private full-packet trace is
  still needed before choosing a correction.
- **#455 did not reproduce in either retained v0.3.4 session.** Queue-full drops
  occur in older v0.3.3 sessions. Absence in two later sessions is not proof that
  the queue-loss defect is fixed.
- **#429 already has the deferred phase-history implementation from PR #463.**
  The latest log contains no armed phase continuation or resume. The original
  duplication defect is closed on the merged implementation and passing
  regression that exercises a phase gap beyond the grace window; this session
  supplies no additional phase validation.
- **#456 remains distinct from #476.** No explicit floor-50 occurrence identifies
  the reported post-boss reset. Fixing inter-wave idle behavior cannot establish
  resolution of that report.
- **#449 has no new base-tier evidence.** The latest log does not enter scene
  5901; do not choose a template merely because its name matches.
- **#425 is closed.** The September 24 v0.3.4 Chaotic Towering Ruin run
  completed cleanly, and the September 25 successful launch confirms the
  previous session completed with workers joined. This satisfies the original
  dungeon plus relaunch-after-close acceptance, though it was not a timed
  immediate-relaunch experiment. Residual transport/queue defects remain
  separately tracked in #454/#455.
- **#274 remains open.** The latest session contains no update transaction;
  it cannot validate the newer cleanup retry fix or the full elevated
  update/rollback/cleanup sequence.

Inspection reported zero writer drops and 7,205 sanitizer rejections. The latter
are intentional exclusions of undecoded or unmodeled records, not a count of
malformed messages. This distinction matters when assessing capture completeness:
a sanitized dump is not a full wire trace.
