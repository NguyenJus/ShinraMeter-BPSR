# Session log findings — 2026-09-24

This note summarizes a locally retained, sanitized v0.3.4 session. It omits
player names, account identifiers, endpoints, and packet payloads.

## Existing issues

- **#425:** the session exercised Chaotic Towering Ruin with the post-v0.3.0
  capture diagnostics present. It reported no capture drops, queue overflow,
  stall-guard recovery, or capture-thread failure. An update handoff released
  the single-instance lock in 0.28 seconds. This is strong field coverage, but
  does not replace the issue's explicit ordinary close-then-relaunch check.
- **#274:** an in-place update installed and relaunched successfully, but the
  successor could not remove the prior executable's `.old` backup because
  Windows returned access denied. Cleanup now retries transient permission
  and Windows sharing failures for a bounded 400 milliseconds per leftover.
  Keep the issue open until a real Windows update validates the cleanup.
- **#454/#455:** neither reproduced in this session. Their prior failures
  remain open because an uneventful session is not a regression proof.

## Fixed history-label defect

Three dungeon exits ended encounters in scene 1150 through `server_changed`,
then recorded history rows as destination scene 7 with no boss label. Duration,
damage, and player-count data remained present, but the history label described
the destination rather than the encounter.

`Pipeline` captures an end-edge record during the post-end grace interval so
trailing combat can be included when it is safe to flush. Before this fix, it
unconditionally rebuilt that record after grace; when the destination `Scene`
arrived in between, the rebuild replaced outgoing metadata. The pipeline now
keeps the already-captured record when its scene differs from the current
meter scene. Same-scene endings still rebuild after grace and retain trailing
combat packets.

The regression is
`pipeline::tests::history_recording::server_change_before_destination_scene_keeps_outgoing_history_metadata`.
