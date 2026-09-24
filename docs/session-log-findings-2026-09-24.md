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
- **#285:** the retained inspector ring supplies the requested fresh field
  evidence: 6 decoded revives, 612,870 `BuffApply` events, 867,506
  `BuffRemove` events, 32 player-targeted damage-death signals, and 27
  player dead-state signals. The two death categories can describe the same
  death and are not a unique-death total. The revive and buff dependencies
  are already closed, so this uncontrolled session is sufficient capture
  coverage without reopening protocol discovery.

## Fixed death-time correction defect

Three player deaths were provisionally closed by attributed outgoing damage
only 70, 71, and 172 milliseconds later; one closing hit was explicitly a
passive skill. The authoritative revive signals arrived 1.9, 61.1, and 7.9
seconds after the deaths. This proves that residual damage can be attributed
to a dead player and is not reliable final evidence of the revive time.

The next-action inference remains as a fallback when no explicit signal is
captured. A later `Revive` or alive state now supersedes that provisional
timestamp, including when the explicit packet arrives behind a newer action.
Ended snapshots cap corrections at the encounter's frozen end, so a late
revive cannot extend a completed attempt. If a curated next phase resumes the
same encounter, the preserved authoritative interval becomes visible again.

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
