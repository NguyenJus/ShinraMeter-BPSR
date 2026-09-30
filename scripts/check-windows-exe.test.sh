#!/usr/bin/env bash
# Exercise output larger than a pipe buffer and failed inspection tools without
# requiring a release build. The executable contents are opaque to these stubs.
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/bin"
touch "$scratch/app.exe"
cat > "$scratch/bin/x86_64-w64-mingw32-objdump" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "${TEST_IMPORT:-DLL Name: KERNEL32.dll}"
printf 'import padding\n%.0s' {1..20000}
exit "${TEST_OBJDUMP_EXIT:-0}"
STUB
cat > "$scratch/bin/strings" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "${TEST_MANIFEST:-requireAdministrator}"
printf 'string padding\n%.0s' {1..20000}
exit "${TEST_STRINGS_EXIT:-0}"
STUB
chmod +x "$scratch/bin/"*
export PATH="$scratch/bin:$PATH"
check="$script_dir/check-windows-exe.sh"
"$check" "$scratch/app.exe"
expect_failure() {
  if "$@" > "$scratch/output" 2>&1; then
    echo "FAIL: expected rejection: $*" >&2
    exit 1
  fi
}
expect_failure env TEST_IMPORT='DLL Name: WinDivert.dll' "$check" "$scratch/app.exe"
expect_failure env TEST_MANIFEST=asInvoker "$check" "$scratch/app.exe"
expect_failure env TEST_OBJDUMP_EXIT=1 "$check" "$scratch/app.exe"
expect_failure env TEST_STRINGS_EXIT=1 "$check" "$scratch/app.exe"
expect_failure "$check" "$scratch/missing.exe"
expect_failure "$check"
echo "all check-windows-exe.sh tests passed"
