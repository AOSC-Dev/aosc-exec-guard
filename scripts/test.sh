#!/usr/bin/env bash
# Local checks for the aosc-exec-guard PoC (no root, no real dialog windows):
#   * unit tests
#   * direct invocation against fabricated ELF files (foreign arch / native / non-ELF)
#   * dialog branch exercised through a stub "zenity"
#
# The kernel-level end-to-end test is separate: sudo scripts/kernel-test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

TMP=tests/tmp
mkdir -p "$TMP"

step() { printf '\n== %s ==\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

step 'cargo test'
cargo test --quiet

step 'cargo build --release'
cargo build --release --quiet
GUARD=$PWD/target/release/aosc-exec-guard

step 'fabricate test files'
# Minimal AArch64 ELF header (e_type=ET_EXEC, e_machine=0xb7): enough for
# binfmt_misc to match it in the kernel test, and for the guard to parse it.
{ printf '\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\xb7\x00\x01\x00\x00\x00'; head -c 236 /dev/zero; } > "$TMP/aarch64.elf"
# Same, but claiming x86_64: "native architecture, yet rejected".
{ printf '\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00\x01\x00\x00\x00'; head -c 236 /dev/zero; } > "$TMP/x86_64.elf"
printf 'just some text\n' > "$TMP/not-an-elf"
chmod +x "$TMP/aarch64.elf" "$TMP/x86_64.elf"   # the kernel test execs these

# Run the guard like a user in a (text) terminal would: no GUI env leaks in.
run_guard() {
  env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE -u INVOCATION_ID "$GUARD" "$@"
}

step 'direct: foreign arch (aarch64) → explain, exit 126'
set +e
out=$(AOSC_EXEC_GUARD_DEBUG=1 env -u DISPLAY -u WAYLAND_DISPLAY -u INVOCATION_ID "$GUARD" "$TMP/aarch64.elf" 2>&1)
code=$?
set -e
printf '%s\nexit=%s\n' "$out" "$code"
[ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
case "$out" in *aarch64*) ;; *) fail 'message should mention aarch64' ;; esac
case "$out" in *x86_64*) ;; *) fail 'message should mention the host arch' ;; esac
case "$out" in *'mode=Text'*) ;; *) fail 'streams are piped and no display is set, so mode should be Text' ;; esac

step 'direct: native-but-rejected → explain, exit 126'
set +e
out=$(run_guard "$TMP/x86_64.elf" 2>&1)
code=$?
set -e
printf '%s\nexit=%s\n' "$out" "$code"
[ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
case "$out" in *损坏*) ;; *) fail 'message should mention corruption' ;; esac

step 'direct: not an ELF → explain, exit 126'
set +e
out=$(run_guard "$TMP/not-an-elf" 2>&1)
code=$?
set -e
printf '%s\nexit=%s\n' "$out" "$code"
[ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
case "$out" in *ELF*) ;; *) fail 'message should mention ELF' ;; esac

step 'dialog branch (stub zenity, no window is opened)'
mkdir -p "$TMP/bin"
cat > "$TMP/bin/zenity" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$AOSC_GUARD_TEST_LOG"
exit 0
STUB
chmod +x "$TMP/bin/zenity"
LOG=$TMP/zenity.log
: > "$LOG"
set +e
env -u INVOCATION_ID DISPLAY=:99 AOSC_GUARD_TEST_LOG="$LOG" AOSC_EXEC_GUARD_DEBUG=1 \
  PATH="$TMP/bin:$PATH" "$GUARD" "$TMP/aarch64.elf" > "$TMP/guard.out" 2> "$TMP/guard.err"
code=$?
set -e
[ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
[ -s "$LOG" ] || fail 'stub zenity was never called'
grep -q 'aarch64' "$LOG" || fail 'dialog text should mention aarch64'
grep -q 'mode=Dialog' "$TMP/guard.err" || fail 'decision should be Dialog in this setup'
echo 'stub zenity received:'
sed 's/^/  /' "$LOG"

if [ -x "$TMP/busybox-aarch64" ]; then
  step 'direct: real aarch64 binary (Alpine busybox-static) → explain'
  set +e
  out=$(run_guard "$TMP/busybox-aarch64" 2>&1)
  code=$?
  set -e
  printf '%s\nexit=%s\n' "$out" "$code"
  [ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
  case "$out" in *aarch64*) ;; *) fail 'message should mention aarch64' ;; esac
else
  step 'real aarch64 binary not present (optional)'
  echo 'run scripts/get-test-binary.sh to fetch busybox-static for aarch64'
fi

step 'local checks passed'
echo 'next: scripts/test.sh && sudo scripts/kernel-test.sh'
