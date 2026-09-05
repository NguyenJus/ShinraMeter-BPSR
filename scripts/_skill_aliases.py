"""Shared skill-icon alias table (issue #348).

`scripts/gen-skill-icons.py` and `scripts/prep-skill-icons.py` both need the
same basename -> canonical-basename map: gen needs it to keep an alias
resolving to shared bytes after its own PNG is deleted, and prep needs it to
know which basenames must *not* get a PNG written (or expected present)
during a refresh. Living here once keeps the two scripts from drifting into
two different alias tables for the same duplicate.
"""

# Basenames whose own PNG has been deleted from `crates/app/assets/skills/`
# because, at deletion time, content hashing confirmed it was byte-identical
# to another committed icon's PNG. Recorded here explicitly, rather than just
# deleting the file, so the basename keeps resolving to (shared) bytes
# instead of silently vanishing — `crates/meter/data/SkillTableIcons.json`
# names some of these aliases directly (e.g. skill id 2900603 ->
# `weapon_sf-01_skill_03`), so dropping the name outright would regress that
# skill's icon to the blank placeholder.
DUPLICATE_ALIASES: dict[str, str] = {
    "weapon_sf-01_skill_03": "weapon_sf-01_kx05",
}
