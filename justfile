# aosc-exec-guard：开发 / 测试 / 安装入口（原先 scripts/*.sh 的内容都在这里）
#
#   just                            # 列出配方
#   just build                      # 静态构建（musl；std 由 rust-toolchain.toml 备好）
#   just test                       # 本地全套检查（无 root）
#   sudo just kernel-test           # 内核端到端（注册 binfmt 条目，结束自动清理）
#   sudo just systemd-install-test  # 真实安装路径（/usr/lib/binfmt.d + systemd-binfmt）
#   sudo just install               # 装到 /usr（重启 systemd-binfmt + exec 冒烟）
#   just install /tmp/pkg aarch64   # 打包/staging：只写文件，指定目标架构（第三个参数 ignore-family）
#   sudo just uninstall             # 卸载（也可带前缀）
#   just get-test-binary            # 下载真实 aarch64 静态二进制（可选，测试会用它）

set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments

# 列出可用配方
default:
    @just --list

# 构建：唯一的 release 二进制，全静态（要拷进空 rootfs / 容器 里直接用，不能带 libc）。
# 直接按当前宿主机选 musl triple；std 由 rust-toolchain.toml 保证装好，
# 真缺了就让它报错（不再回退 glibc）。
# 产物固定在 target/static/aosc-exec-guard——conf 里的 /usr/bin/aosc-exec-guard 就是它。
build:
    #!/usr/bin/env bash
    set -euo pipefail
    host=$(rustc -vV | awk '/^host:/ {print $2}')
    case "$host" in
      *-musl) triple=$host ;;
      *) triple=${host%-gnu}-musl ;;   # x86_64-unknown-linux-gnu → …-linux-musl
    esac
    cargo build --release --target "$triple"
    install -Dm755 "target/$triple/release/aosc-exec-guard" target/static/aosc-exec-guard
    file target/static/aosc-exec-guard

# 代码检查：rustfmt + clippy
check:
    cargo fmt --check
    cargo clippy --all-targets --quiet

# 本地全套检查（无 root）：单测 + 直接调用 + stub 弹框/仿真器 + installer
test: build
    #!/usr/bin/env bash
    set -euo pipefail

    TMP=tests/tmp
    mkdir -p "$TMP" "$TMP/home" "$TMP/binfmt-empty"

    step() { printf '\n== %s ==\n' "$*"; }
    fail() { echo "FAIL: $*" >&2; exit 1; }

    step 'cargo test'
    cargo test --quiet

    GUARD=$PWD/target/static/aosc-exec-guard

    step 'fabricate test files'
    # Minimal AArch64 ELF header (e_type=ET_EXEC, e_machine=0xb7): enough for
    # binfmt_misc to match it in the kernel test, and for the guard to parse it.
    { printf '\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\xb7\x00\x01\x00\x00\x00'; head -c 236 /dev/zero; } > "$TMP/aarch64.elf"
    # Same, but claiming x86_64: "native architecture, yet rejected".
    { printf '\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00\x01\x00\x00\x00'; head -c 236 /dev/zero; } > "$TMP/x86_64.elf"
    printf 'just some text\n' > "$TMP/not-an-elf"
    chmod +x "$TMP/aarch64.elf" "$TMP/x86_64.elf"   # the kernel test execs these

    # Environment isolation for direct guard runs: no GUI/service context, a
    # private HOME (so a saved “不再询问” answer cannot leak in), an explicit
    # binfmt dir (so a host-installed qemu entry cannot change the outcome)
    # and a pinned language (so the assertions do not depend on the locale).
    # stdin is /dev/null, so the terminal prompt never blocks a scripted run.
    guard_env() { # $1 = binfmt dir, rest = command
      local binfmt_dir=$1
      shift
      env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE -u INVOCATION_ID -u XDG_CONFIG_HOME \
        HOME="$PWD/$TMP/home" AOSC_EXEC_GUARD_BINFMT_DIR="$binfmt_dir" AOSC_EXEC_GUARD_LANG=zh_CN \
        "$@" </dev/null
    }
    run_guard() { guard_env "$PWD/$TMP/binfmt-empty" "$GUARD" "$@"; }
    run_guard_qemu() { guard_env "$PWD/$TMP/binfmt" "$GUARD" "$@"; }

    step 'direct: foreign arch (aarch64) → explain, exit 126'
    set +e
    out=$(AOSC_EXEC_GUARD_DEBUG=1 run_guard "$TMP/aarch64.elf" 2>&1)
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

    step 'i18n: AOSC_EXEC_GUARD_LANG=en 时输出英文'
    set +e
    out=$(env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_CONFIG_HOME HOME="$PWD/$TMP/home" \
      AOSC_EXEC_GUARD_BINFMT_DIR="$PWD/$TMP/binfmt-empty" AOSC_EXEC_GUARD_LANG=en \
      "$GUARD" "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf '%s\nexit=%s\n' "$out" "$code"
    [ "$code" -eq 126 ] || fail "English run should still exit 126, got $code"
    case "$out" in *'cannot run'*) ;; *) fail 'AOSC_EXEC_GUARD_LANG=en should give English text' ;; esac
    case "$out" in *无法*) fail 'English output should not contain Chinese text' ;; esac

    step 'CLI: --help / --version / missing target'
    "$GUARD" --help >/dev/null || fail '--help should exit 0'
    "$GUARD" --version >/dev/null || fail '--version should exit 0'
    set +e
    "$GUARD" >/dev/null 2>&1
    code=$?
    set -e
    [ "$code" -eq 2 ] || fail "missing target should exit 2 (usage error), got $code"

    step 'CLI: --debug is recognised before the program path'
    set +e
    out=$(run_guard --debug "$TMP/not-an-elf" 2>&1)
    code=$?
    set -e
    [ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
    case "$out" in *'[debug] mode='*) ;; *) fail '--debug before the path should print the decision' ;; esac

    step 'CLI: kernel-style arguments stay pass-through'
    # binfmt_misc appends the original program's arguments; they must never be
    # parsed as guard options.
    set +e
    out=$(run_guard "$TMP/not-an-elf" --debug --help -l 你好 2>&1)
    code=$?
    set -e
    [ "$code" -eq 126 ] || fail "pass-through arguments should still exit 126, got $code"
    case "$out" in *'[debug]'*) fail '--debug after the path must not reach the guard' ;; esac
    case "$out" in *'Usage:'*) fail '--help after the path must not show the guard help' ;; esac

    step 'CLI: non-UTF-8 argument does not break the guard'
    set +e
    out=$(run_guard "$TMP/not-an-elf" "$(printf '\xff')" 2>&1)
    code=$?
    set -e
    [ "$code" -eq 126 ] || fail "non-UTF-8 argument should still exit 126, got $code"
    case "$out" in *ELF*) ;; *) fail 'explanation should still be printed' ;; esac

    step 'installer: 按家族过滤规则（可交叉打包）'
    # x86_64 目标：整个 i386 家族（i386 + x86_64）都不该留下
    just install "$TMP/pkg-x86_64" x86_64 > "$TMP/install.log" 2>&1 \
      || fail "installer 在 x86_64 目标上失败：$(cat "$TMP/install.log")"
    conf=$TMP/pkg-x86_64/lib/binfmt.d/zz-aosc-exec-guard.conf
    [ -x "$TMP/pkg-x86_64/bin/aosc-exec-guard" ] || fail 'installer 没装二进制'
    case "$(cat "$conf")" in
      *':aosc-exec-guard-i386:'*|*':aosc-exec-guard-x86_64:'*)
        fail 'x86_64 目标的 conf 里不该有 i386/x86_64 规则'
        ;;
    esac
    case "$(cat "$conf")" in
      *':aosc-exec-guard-aarch64:'*) ;;
      *) fail 'x86_64 目标的 conf 里应保留 aarch64 规则' ;;
    esac
    # aarch64 目标：arm 家族（arm + aarch64）都不该留下，x86_64 要留着
    just install "$TMP/pkg-aarch64" aarch64 > /dev/null 2>&1 \
      || fail 'installer 在 aarch64 目标上失败'
    conf=$TMP/pkg-aarch64/lib/binfmt.d/zz-aosc-exec-guard.conf
    case "$(cat "$conf")" in
      *':aosc-exec-guard-arm:'*|*':aosc-exec-guard-aarch64:'*)
        fail 'aarch64 目标的 conf 里不该有 arm/aarch64 规则'
        ;;
    esac
    case "$(cat "$conf")" in
      *':aosc-exec-guard-x86_64:'*) ;;
      *) fail 'aarch64 目标的 conf 里应保留 x86_64 规则' ;;
    esac
    # 默认（本机架构）：自检和过滤都不应该报错
    just install "$TMP/pkg" > /dev/null || fail 'installer 在本机架构上失败'
    conf=$TMP/pkg/lib/binfmt.d/zz-aosc-exec-guard.conf
    echo "本机 $(uname -m)：过滤后剩 $(grep -c '^:' "$conf") 条规则"
    [ -f "$TMP/pkg/share/aosc-exec-guard/dialog.qml" ] || fail 'installer 没装 Kirigami 弹框的 QML'
    # 卸载
    just uninstall "$TMP/pkg" > /dev/null || fail 'uninstall 失败'
    [ ! -e "$conf" ] || fail 'uninstall 没删 conf'
    [ ! -e "$TMP/pkg/bin/aosc-exec-guard" ] || fail 'uninstall 没删二进制'
    [ ! -e "$TMP/pkg/share/aosc-exec-guard/dialog.qml" ] || fail 'uninstall 没删 QML'

    step 'installer: --alternatives（box64 式：conf 进 binfmt.alternatives + 槽位声明）'
    rm -rf "$TMP/pkg-alt"
    scripts/install.sh --prefix "$TMP/pkg-alt" --alternatives --priority 100 \
      > "$TMP/install-alt.log" 2>&1 \
      || fail "install.sh --alternatives 失败：$(cat "$TMP/install-alt.log")"
    [ -d "$TMP/pkg-alt/lib/binfmt.alternatives" ] || fail 'alternatives 模式该把 conf 放 lib/binfmt.alternatives/'
    [ ! -e "$TMP/pkg-alt/lib/binfmt.d/zz-aosc-exec-guard.conf" ] || fail 'alternatives 模式不该再有普通 conf'
    [ ! -e "$TMP/pkg-alt/lib/binfmt.d/emu-x86_64.conf" ] || fail '本机家族的槽位不该出现（会被过滤掉）'
    for arch in aarch64 riscv64 loongarch64; do
      conf=$TMP/pkg-alt/lib/binfmt.alternatives/aosc-exec-guard-$arch.conf
      [ -f "$conf" ] || fail "缺少 $arch 的 conf"
      [ "$(grep -c '^:' "$conf")" -eq 1 ] || fail "$arch 的 conf 应该只有 1 条规则（别的都进别的文件）"
      grep -q "^:aosc-exec-guard-$arch:" "$conf" || fail "$arch 的 conf 里规则名不对"
    done
    grep -q '^alternative /usr/lib/binfmt.d/emu-aarch64.conf /usr/lib/binfmt.alternatives/aosc-exec-guard-aarch64.conf 100$' \
      "$TMP/pkg-alt/share/aosc-exec-guard/alternatives" \
      || fail 'alternatives 声明里应有 emu-aarch64.conf 那一行（路径按装好后的 /usr 写）'
    scripts/install.sh --prefix "$TMP/pkg-alt" --uninstall > /dev/null \
      || fail 'alternatives 模式的 staging 卸载失败'
    [ ! -e "$TMP/pkg-alt/lib/binfmt.alternatives/aosc-exec-guard-aarch64.conf" ] \
      || fail 'staging 卸载没删 alternatives 里的 conf'

    step 'stubs: fake binfmt dir + stub qemu + stub zenity'
    mkdir -p "$TMP/bin" "$TMP/binfmt"
    cat > "$TMP/bin/qemu-aarch64" <<'STUB'
    #!/usr/bin/env bash
    printf 'stub-qemu %s\n' "$*"
    exit 42
    STUB
    chmod +x "$TMP/bin/qemu-aarch64"
    cat > "$TMP/binfmt/qemu-aarch64" <<EOF
    enabled
    interpreter $PWD/$TMP/bin/qemu-aarch64
    flags: OCF
    EOF
    # Stub zenity: logs its arguments to $AOSC_GUARD_TEST_LOG, and can pretend the
    # user ticked “不再询问” or cancelled the dialog.
    cat > "$TMP/bin/zenity" <<'STUB'
    #!/usr/bin/env bash
    printf '%s\n' "$@" >> "$AOSC_GUARD_TEST_LOG"
    [ -n "${AOSC_GUARD_TEST_ZENITY_CANCEL:-}" ] && exit 1
    [ -n "${AOSC_GUARD_TEST_ZENITY_CHECKED:-}" ] && printf '不再询问\n'
    exit 0
    STUB
    chmod +x "$TMP/bin/zenity"
    LOG=$TMP/zenity.log
    : > "$LOG"

    step 'dialog branch (stub zenity, no window is opened)'
    set +e
    env -u INVOCATION_ID -u XDG_CONFIG_HOME -u XDG_CURRENT_DESKTOP -u XDG_SESSION_DESKTOP \
      -u DESKTOP_SESSION -u KDE_FULL_SESSION DISPLAY=:99 AOSC_GUARD_TEST_LOG="$LOG" \
      AOSC_EXEC_GUARD_DEBUG=1 \
      HOME="$PWD/$TMP/home" AOSC_EXEC_GUARD_BINFMT_DIR="$PWD/$TMP/binfmt-empty" AOSC_EXEC_GUARD_LANG=zh_CN \
      PATH="$TMP/bin:$PATH" "$GUARD" "$TMP/aarch64.elf" </dev/null > "$TMP/guard.out" 2> "$TMP/guard.err"
    code=$?
    set -e
    [ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
    [ -s "$LOG" ] || fail 'stub zenity was never called'
    grep -q 'aarch64' "$LOG" || fail 'dialog text should mention aarch64'
    grep -q 'mode=Dialog' "$TMP/guard.err" || fail 'decision should be Dialog in this setup'
    grep -q 'dialog=zenity' "$TMP/guard.err" || fail '非 KDE 会话应当先试 zenity'
    echo 'stub zenity received:'
    sed 's/^/  /' "$LOG"

    step 'qemu: never → explain only, no emulator'
    set +e
    out=$(AOSC_EXEC_GUARD_QEMU=never run_guard_qemu "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf '%s\nexit=%s\n' "$out" "$code"
    [ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
    case "$out" in *'qemu-aarch64'*) ;; *) fail 'message should mention the installed qemu-aarch64' ;; esac
    case "$out" in *'stub-qemu'*) fail 'qemu must not run in never mode' ;; esac

    step 'qemu: always → hand the program (and its arguments) to the emulator'
    set +e
    out=$(AOSC_EXEC_GUARD_QEMU=always run_guard_qemu "$TMP/aarch64.elf" one "two words" --flag 2>&1)
    code=$?
    set -e
    printf '%s\nexit=%s\n' "$out" "$code"
    [ "$code" -eq 42 ] || fail "exit status should come from the stub qemu (42), got $code"
    case "$out" in *'stub-qemu'*) ;; *) fail 'stub qemu should have been used' ;; esac
    case "$out" in *'aarch64.elf one two words --flag'*) ;; *) fail 'program arguments should be forwarded verbatim' ;; esac

    step 'qemu: 槽位归 guard（box64 式）时，模拟器从 binfmt.alternatives 的候选里找'
    # 假注册表：可见（有 register），但里面没有 qemu 条目——就像 guard 自己的 conf
    # 占着 /usr/lib/binfmt.d/emu-aarch64.conf 那个槽位的样子。
    mkdir -p "$TMP/binfmt-alt" "$TMP/alt"
    : > "$TMP/binfmt-alt/register"
    cat > "$TMP/alt/qemu-aarch64.conf" <<EOF
    :qemu-aarch64:M::\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\xb7\x00:\xff\xff\xff\xff\xff\xff\xff\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff:$PWD/$TMP/bin/qemu-aarch64:CF
    EOF
    set +e
    out=$(AOSC_EXEC_GUARD_BINFMT_DIR="$PWD/$TMP/binfmt-alt" \
      AOSC_EXEC_GUARD_ALTERNATIVES_DIR="$PWD/$TMP/alt" \
      AOSC_EXEC_GUARD_QEMU=always "$GUARD" "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf '%s\nexit=%s\n' "$out" "$code"
    [ "$code" -eq 42 ] \
      || fail "注册表里没有条目时应退到 alternatives 的候选并交给它跑（期望 42，实际 $code）"
    case "$out" in *'stub-qemu'*) ;; *) fail '应该通过 alternatives 候选里的 stub qemu 运行' ;; esac

    step 'qemu: ask without any way to ask → keep the old behavior (just run it)'
    set +e
    out=$(AOSC_EXEC_GUARD_QEMU=ask run_guard_qemu "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf '%s\nexit=%s\n' "$out" "$code"
    [ "$code" -eq 42 ] || fail "no tty and no GUI: the stub qemu should have run, got $code"

    step 'qemu: terminal menu (pty, dialoguer): 运行 / 不运行 / 总是'
    cat > "$TMP/bin/ask-guard" <<EOF
    #!/usr/bin/env bash
    export HOME="$PWD/$TMP/home" AOSC_EXEC_GUARD_BINFMT_DIR="$PWD/$TMP/binfmt" AOSC_EXEC_GUARD_QEMU=ask AOSC_EXEC_GUARD_LANG=zh_CN
    unset DISPLAY WAYLAND_DISPLAY XDG_SESSION_TYPE INVOCATION_ID XDG_CONFIG_HOME
    exec "$GUARD" "\$@"
    EOF
    chmod +x "$TMP/bin/ask-guard"
    ask_pty() { # $1 = 按键（j/k 上下移动，回车确认）；sets $out and $code
      rm -f "$PWD/$TMP/home/.config/aosc-exec-guard.conf"
      set +e
      printf '%b' "$1" | script -qec "$PWD/$TMP/bin/ask-guard $TMP/aarch64.elf" /dev/null \
        > "$TMP/pty.out" 2>&1
      code=$?
      set -e
      out=$(tr -d '\r' < "$TMP/pty.out")
      printf '%s\n' "$out"
      printf 'keys=%s exit=%s\n' "$1" "$code"
    }
    ask_pty '\n'   # 默认项是“不运行（这次）”
    [ "$code" -eq 126 ] || fail "the default item should explain and exit 126, got $code"
    case "$out" in *'无法运行'*) ;; *) fail 'declining should print the explanation' ;; esac
    case "$out" in *'总是运行（不再询问）'*) ;; *) fail 'the dialoguer menu should have been drawn' ;; esac
    case "$out" in *'❯'*) ;; *) fail '菜单该用 dialoguer 自带的 ColorfulTheme（❯ 指着当前项）' ;; esac
    case "$out" in *'总是不运行'*) fail '菜单里不该再有“总是不运行”（要固定 never 用配置/环境变量）' ;; esac
    ask_pty 'k\n'  # 上移一项 → 运行（这次）
    [ "$code" -eq 42 ] || fail "choosing “run once” should run the stub qemu, got $code"
    case "$out" in *'stub-qemu'*) ;; *) fail 'the menu should lead to the stub qemu' ;; esac
    ask_pty 'j\n'  # 下移一项 → 总是运行（不再询问）
    [ "$code" -eq 42 ] || fail "choosing “always run” should run the stub qemu, got $code"
    grep -q 'qemu = always' "$PWD/$TMP/home/.config/aosc-exec-guard.conf" \
      || fail 'choosing “always run” should be remembered'
    ask_pty 'jj\n' # 3 项会绕圈：默认(1) → 2 总是运行 → 0 运行（这次）
    [ "$code" -eq 42 ] || fail "wrapping around the 3-item menu should still run, got $code"
    [ ! -e "$PWD/$TMP/home/.config/aosc-exec-guard.conf" ] \
      || fail 'wrapping to “run once” must not be remembered'
    ask_pty 'q'    # q 退出菜单 = 这次不运行（不记住）
    [ "$code" -eq 126 ] || fail "quitting the menu should explain and exit 126, got $code"
    [ ! -e "$PWD/$TMP/home/.config/aosc-exec-guard.conf" ] || fail 'quitting must not be remembered'

    step 'qemu: GUI checkbox (stub zenity) is only remembered when ticked'
    rm -f "$PWD/$TMP/home/.config/aosc-exec-guard.conf"
    run_gui_ask() {
      : > "$LOG"
      set +e
      env -u INVOCATION_ID -u XDG_CONFIG_HOME -u XDG_CURRENT_DESKTOP -u XDG_SESSION_DESKTOP \
        -u DESKTOP_SESSION -u KDE_FULL_SESSION DISPLAY=:99 AOSC_GUARD_TEST_LOG="$LOG" \
        HOME="$PWD/$TMP/home" AOSC_EXEC_GUARD_BINFMT_DIR="$PWD/$TMP/binfmt" AOSC_EXEC_GUARD_QEMU=ask \
        AOSC_EXEC_GUARD_LANG=zh_CN \
        PATH="$TMP/bin:$PATH" "$@" "$GUARD" "$TMP/aarch64.elf" </dev/null > "$TMP/g.out" 2> "$TMP/g.err"
      code=$?
      set -e
      printf 'exit=%s\n' "$code"
    }
    run_gui_ask env  # 没勾选框：本次运行，不记住
    [ "$code" -eq 42 ] || fail "the dialog should have run the stub qemu, got $code"
    grep -q -- '--checklist' "$LOG" || fail 'zenity should get the checklist dialog'
    [ ! -e "$PWD/$TMP/home/.config/aosc-exec-guard.conf" ] || fail 'unticked box must not be remembered'
    run_gui_ask env AOSC_GUARD_TEST_ZENITY_CHECKED=1
    [ "$code" -eq 42 ] || fail "the dialog should have run the stub qemu, got $code"
    grep -q 'qemu = always' "$PWD/$TMP/home/.config/aosc-exec-guard.conf" \
      || fail 'ticked box should remember “always run”'
    rm -f "$PWD/$TMP/home/.config/aosc-exec-guard.conf"
    run_gui_ask env AOSC_GUARD_TEST_ZENITY_CANCEL=1
    [ "$code" -eq 126 ] || fail "cancelling should explain and exit 126, got $code"
    grep -q -- '--error' "$LOG" || fail 'declining should end in the explanation dialog'

    step 'qemu: Kirigami 弹框（KDE 会话下优先，stub 运行时验参数/结果/回退）'
    cat > "$TMP/bin/qml-stub" <<'STUB'
    #!/usr/bin/env bash
    printf '%s\n' "$@" >> "$AOSC_GUARD_TEST_QML_LOG"
    case "${AOSC_GUARD_TEST_QML_ANSWER:-run}" in
      run) exit 10 ;;
      remember) exit 12 ;;
      decline) exit 11 ;;
      *) exit 2 ;;  # qml 运行时自己出错时的码
    esac
    STUB
    chmod +x "$TMP/bin/qml-stub"
    QLOG=$TMP/qml.log
    rm -f "$PWD/$TMP/home/.config/aosc-exec-guard.conf"
    run_kirigami() { # $1 = qml 的答案（run/remember/decline/broken）
      : > "$QLOG"
      : > "$LOG"
      set +e
      out=$(env -u INVOCATION_ID -u XDG_CONFIG_HOME DISPLAY=:99 \
        AOSC_GUARD_TEST_LOG="$LOG" AOSC_GUARD_TEST_QML_LOG="$QLOG" AOSC_GUARD_TEST_QML_ANSWER="$1" \
        XDG_CURRENT_DESKTOP=KDE AOSC_EXEC_GUARD_QML="$PWD/$TMP/bin/qml-stub" \
        AOSC_EXEC_GUARD_QML_FILE="$PWD/data/dialog.qml" AOSC_EXEC_GUARD_QEMU=ask \
        HOME="$PWD/$TMP/home" AOSC_EXEC_GUARD_BINFMT_DIR="$PWD/$TMP/binfmt" \
        AOSC_EXEC_GUARD_LANG=zh_CN PATH="$TMP/bin:$PATH" \
        "$GUARD" "$TMP/aarch64.elf" 2>&1)
      code=$?
      set -e
      printf 'qml-answer=%s exit=%s\n' "$1" "$code"
    }

    run_kirigami run
    [ "$code" -eq 42 ] || fail "qml 回“运行”就该交给 stub qemu（42），实际 $code"
    grep -qx -- '--' "$QLOG" || fail '参数要在 `--` 之后传给 qml 运行时（否则会被当成 QML 文件）'
    grep -q 'aarch64' "$QLOG" || fail 'QML 弹框应该收到问题正文'
    grep -q '不再询问' "$QLOG" || fail 'QML 弹框应该收到本地化的复选框文字'
    grep -q '运行' "$QLOG" || fail 'QML 弹框应该收到按钮文字'
    [ ! -s "$LOG" ] || fail 'KDE 会话里该先试 Kirigami 弹框，不该轮到 zenity'

    run_kirigami remember
    [ "$code" -eq 42 ] || fail 'qml 回“运行并记住”也该交给 stub qemu'
    grep -q 'qemu = always' "$PWD/$TMP/home/.config/aosc-exec-guard.conf" \
      || fail 'qml 回“运行并记住”应该写进配置'
    rm -f "$PWD/$TMP/home/.config/aosc-exec-guard.conf"

    run_kirigami decline
    [ "$code" -eq 126 ] || fail 'qml 回“不运行”应该只解释、退 126'

    run_kirigami broken   # 比如 QML 加载失败：qml 退出码不是约定，应该退回 zenity
    [ "$code" -eq 42 ] || fail 'qml 坏掉时应该退回 zenity 并交给 stub qemu'
    [ -s "$LOG" ] || fail 'qml 出问题时应该退回 zenity'

    step 'qemu: 非 KDE 会话用 zenity，不碰 Kirigami 弹框'
    : > "$LOG"; : > "$QLOG"
    set +e
    out=$(env -u INVOCATION_ID -u XDG_CONFIG_HOME -u XDG_CURRENT_DESKTOP -u XDG_SESSION_DESKTOP \
      -u DESKTOP_SESSION -u KDE_FULL_SESSION DISPLAY=:99 \
      AOSC_GUARD_TEST_LOG="$LOG" AOSC_GUARD_TEST_QML_LOG="$QLOG" \
      AOSC_EXEC_GUARD_QML="$PWD/$TMP/bin/qml-stub" AOSC_EXEC_GUARD_QML_FILE="$PWD/data/dialog.qml" \
      AOSC_EXEC_GUARD_QEMU=ask HOME="$PWD/$TMP/home" AOSC_EXEC_GUARD_BINFMT_DIR="$PWD/$TMP/binfmt" \
      AOSC_EXEC_GUARD_LANG=zh_CN PATH="$TMP/bin:$PATH" "$GUARD" "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    [ "$code" -eq 42 ] || fail "zenity 回“运行”就该交给 stub qemu，实际 $code"
    [ -s "$LOG" ] || fail '非 KDE 会话应该用 zenity'
    [ ! -s "$QLOG" ] || fail '非 KDE 会话不该动 Kirigami 弹框'

    step 'qemu: AOSC_EXEC_GUARD_DIALOG 可以钉死弹框工具'
    : > "$LOG"; : > "$QLOG"
    set +e
    out=$(env -u INVOCATION_ID -u XDG_CONFIG_HOME DISPLAY=:99 \
      AOSC_GUARD_TEST_LOG="$LOG" AOSC_GUARD_TEST_QML_LOG="$QLOG" \
      XDG_CURRENT_DESKTOP=KDE AOSC_EXEC_GUARD_DIALOG=zenity \
      AOSC_EXEC_GUARD_QML="$PWD/$TMP/bin/qml-stub" AOSC_EXEC_GUARD_QML_FILE="$PWD/data/dialog.qml" \
      AOSC_EXEC_GUARD_QEMU=ask HOME="$PWD/$TMP/home" AOSC_EXEC_GUARD_BINFMT_DIR="$PWD/$TMP/binfmt" \
      AOSC_EXEC_GUARD_LANG=zh_CN PATH="$TMP/bin:$PATH" "$GUARD" "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    [ "$code" -eq 42 ] || fail "钉死 zenity 后应该走 zenity，实际 $code"
    [ -s "$LOG" ] || fail 'AOSC_EXEC_GUARD_DIALOG=zenity 应该用 zenity'
    [ ! -s "$QLOG" ] || fail 'AOSC_EXEC_GUARD_DIALOG=zenity 时不该试 Kirigami 弹框'

    step 'Kirigami 弹框：真 QML + 真 qml 运行时（offscreen，不弹窗）'
    QML_RUNTIME=/usr/lib/qt6/bin/qml
    if [ -x "$QML_RUNTIME" ]; then
      for pair in run:10 remember:12 decline:11; do
        set +e
        QT_QPA_PLATFORM=offscreen "$QML_RUNTIME" "$PWD/data/dialog.qml" -- \
          --title 测试 --text 正文 --checkbox 不再询问 --ok 运行 --cancel 不运行 \
          --test-answer "${pair%%:*}" >/dev/null 2>&1
        code=$?
        set -e
        [ "$code" -eq "${pair##*:}" ] \
          || fail "真 QML 的 ${pair%%:*} 应该退 ${pair##*:}，实际 $code"
      done
      echo '=> 真 QML 的退出码对得上（10/12/11）'
    else
      echo "没装 Qt6 的 qml 运行时（$QML_RUNTIME），跳过真 QML 检查"
    fi

    step 'qemu: saved answer wins, --qemu overrides it'
    printf 'qemu = always\n' > "$PWD/$TMP/home/.config/aosc-exec-guard.conf"
    set +e
    out=$(run_guard_qemu "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf 'config=always exit=%s\n' "$code"
    [ "$code" -eq 42 ] || fail "a saved “always” should run the stub qemu, got $code"
    set +e
    out=$(run_guard_qemu --qemu=never "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf 'config=always + --qemu=never exit=%s\n' "$code"
    [ "$code" -eq 126 ] || fail "--qemu=never should override the saved answer, got $code"
    rm -f "$PWD/$TMP/home/.config/aosc-exec-guard.conf"

    step 'qemu: /etc 里的系统默认（用户配置盖过它）'
    printf '# 系统默认（打包方/管理员放的）\nqemu = never\n' > "$TMP/etc-guard.conf"
    printf 'qemu = always\n' > "$PWD/$TMP/home/.config/aosc-exec-guard.conf"
    set +e
    out=$(guard_env "$PWD/$TMP/binfmt" AOSC_EXEC_GUARD_SYSTEM_CONFIG="$PWD/$TMP/etc-guard.conf" "$GUARD" "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf 'user=always + system=never exit=%s\n' "$code"
    [ "$code" -eq 42 ] || fail "用户配置应盖过 /etc 的系统默认，got $code"
    rm -f "$PWD/$TMP/home/.config/aosc-exec-guard.conf"
    set +e
    out=$(guard_env "$PWD/$TMP/binfmt" AOSC_EXEC_GUARD_SYSTEM_CONFIG="$PWD/$TMP/etc-guard.conf" "$GUARD" "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf 'user=- + system=never exit=%s\n' "$code"
    [ "$code" -eq 126 ] || fail "/etc 的系统默认应生效（never → 只解释），got $code"

    step 'qemu: chroot 里让位（忽略保存的选择、不询问）'
    # 真 chroot 的用例在 kernel-test；这里用 AOSC_EXEC_GUARD_FORCE_CHROOT 让 guard
    # 以为自己在 chroot 里。
    run_chroot() { guard_env "$PWD/$TMP/binfmt" AOSC_EXEC_GUARD_FORCE_CHROOT=1 "$@"; }

    printf 'qemu = never\n' > "$PWD/$TMP/home/.config/aosc-exec-guard.conf"
    set +e
    out=$(run_chroot "$GUARD" "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf 'chroot + config=never exit=%s\n' "$code"
    [ "$code" -eq 42 ] || fail "chroot 里应忽略宿主机保存的选择、直接交给模拟器，got $code"
    case "$out" in *'stub-qemu'*) ;; *) fail 'chroot 里应该转发给 stub qemu' ;; esac

    set +e
    out=$(run_chroot AOSC_EXEC_GUARD_QEMU=never "$GUARD" "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf 'chroot + env=never exit=%s\n' "$code"
    [ "$code" -eq 126 ] || fail "显式的 AOSC_EXEC_GUARD_QEMU=never 在 chroot 里也该算数，got $code"

    set +e
    out=$(guard_env "$PWD/$TMP/binfmt-empty" AOSC_EXEC_GUARD_FORCE_CHROOT=1 "$GUARD" "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf 'chroot + 没有模拟器 exit=%s\n' "$code"
    [ "$code" -eq 126 ] || fail "chroot 里找不到模拟器时应解释并退 126，got $code"
    case "$out" in *chroot*) ;; *) fail 'chroot 里的提示应该提到 chroot' ;; esac

    # 对照：同样的配置在宿主机（不在 chroot）里仍然生效
    set +e
    out=$(run_guard_qemu "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf 'host + config=never exit=%s\n' "$code"
    [ "$code" -eq 126 ] || fail "宿主机上保存的选择仍然有效，got $code"
    rm -f "$PWD/$TMP/home/.config/aosc-exec-guard.conf"

    step 'qemu: 看不到注册表（没挂 /proc 的 chroot、容器）时不询问、直接解释'
    # guard 只认 binfmt_misc 注册表（本地或宿主机的）；两边都看不见/没有条目就
    # 解释退出，而且即使在终端里也不能弹菜单（不猜 /usr/bin 路径）。
    set +e
    printf '\n' | script -qec "env -u DISPLAY -u WAYLAND_DISPLAY -u INVOCATION_ID -u XDG_CONFIG_HOME HOME=$PWD/$TMP/home AOSC_EXEC_GUARD_BINFMT_DIR=$PWD/$TMP/no-such-dir AOSC_EXEC_GUARD_LANG=zh_CN $GUARD $TMP/aarch64.elf" /dev/null \
      > "$TMP/pty2.out" 2>&1
    code=$?
    set -e
    out=$(tr -d '\r' < "$TMP/pty2.out")
    printf '%s\nexit=%s\n' "$out" "$code"
    case "$out" in *'运行（这次）'*) fail '看不到注册表时不该弹询问菜单' ;; esac
    [ "$code" -eq 126 ] || fail "看不到注册表时应解释并退 126，实际 $code"
    case "$out" in *'无法运行'*) ;; *) fail '应该给出解释' ;; esac
    case "$out" in *'--handover'*) ;; *) fail '解释里应给出 --handover 这条出路' ;; esac

    step 'handover: 宿主上下文之外拒绝'
    set +e
    out=$(env AOSC_EXEC_GUARD_BINFMT_DIR="$PWD/$TMP/no-such-dir" AOSC_EXEC_GUARD_LANG=zh_CN "$GUARD" --handover --yes 2>&1)
    code=$?
    set -e
    printf '%s\nexit=%s\n' "$out" "$code"
    [ "$code" -eq 1 ] || fail "看不到注册表时应拒绝，实际 $code"
    case "$out" in *外面*) ;; *) fail '应提示到宿主机（外面）运行' ;; esac

    set +e
    out=$(guard_env "$PWD/$TMP/binfmt" AOSC_EXEC_GUARD_FORCE_CHROOT=1 "$GUARD" --handover --yes 2>&1)
    code=$?
    set -e
    [ "$code" -eq 1 ] || fail "chroot 里应拒绝，实际 $code"
    case "$out" in *chroot*外面*) ;; *) fail '应提示 chroot 里做不了、到外面运行' ;; esac

    set +e
    out=$(guard_env "$PWD/$TMP/binfmt" AOSC_EXEC_GUARD_FORCE_CONTAINER=1 "$GUARD" --handover --yes 2>&1)
    code=$?
    set -e
    [ "$code" -eq 1 ] || fail "容器里应拒绝，实际 $code"
    case "$out" in *容器*外面*) ;; *) fail '应提示容器里做不了、到外面运行' ;; esac

    step 'handover: 注销条目 + 停用 conf（测试目录走完整流程）'
    mkdir -p "$TMP/hand/binfmt" "$TMP/hand/conf"
    printf 'enabled\ninterpreter /bin/true\nflags: F\n' > "$TMP/hand/binfmt/aosc-exec-guard-aarch64"
    printf 'enabled\ninterpreter %s\nflags: OCF\n' "$PWD/$TMP/bin/qemu-aarch64" > "$TMP/hand/binfmt/qemu-aarch64"
    printf '# 测试用 conf（让位只改名、不解析内容）\n:aosc-exec-guard-aarch64:M::x:/usr/bin/aosc-exec-guard:F\n' \
      > "$TMP/hand/conf/zz-aosc-exec-guard.conf"
    hand_env() { env AOSC_EXEC_GUARD_BINFMT_DIR="$PWD/$TMP/hand/binfmt" AOSC_EXEC_GUARD_CONF_DIRS="$PWD/$TMP/hand/conf" AOSC_EXEC_GUARD_LANG=zh_CN "$@"; }

    set +e
    out=$(hand_env "$GUARD" --handover --yes 2>&1)
    code=$?
    set -e
    printf '%s\nexit=%s\n' "$out" "$code"
    [ "$code" -eq 0 ] || fail "让位应该成功，实际 $code"
    [ ! -e "$TMP/hand/conf/zz-aosc-exec-guard.conf" ] || fail 'conf 应被改名'
    [ -e "$TMP/hand/conf/zz-aosc-exec-guard.conf.disabled" ] || fail 'conf 应改名为 .disabled'
    [ ! -e "$TMP/hand/binfmt/aosc-exec-guard-aarch64" ] || fail 'guard 条目应被注销'
    [ -e "$TMP/hand/binfmt/qemu-aarch64" ] || fail 'qemu 条目不该被动'
    case "$out" in *'让位完成'*) ;; *) fail '应报告让位完成' ;; esac

    set +e
    out=$(hand_env "$GUARD" --handover --yes 2>&1)
    code=$?
    set -e
    [ "$code" -eq 1 ] || fail "已在让位状态时应直接告知，实际 $code"
    case "$out" in *off*) ;; *) fail '应提示用 --handover=off 恢复' ;; esac

    step 'handover: =off 恢复（测试目录）'
    set +e
    out=$(hand_env "$GUARD" --handover=off --yes 2>&1)
    code=$?
    set -e
    printf '%s\nexit=%s\n' "$out" "$code"
    [ "$code" -eq 0 ] || fail "恢复应该成功，实际 $code"
    [ -e "$TMP/hand/conf/zz-aosc-exec-guard.conf" ] || fail 'conf 应恢复原名'
    [ ! -e "$TMP/hand/conf/zz-aosc-exec-guard.conf.disabled" ] || fail '.disabled 应已消失'

    step 'handover: 没有终端又不加 --yes 时拒绝'
    set +e
    out=$(hand_env "$GUARD" --handover 2>&1 </dev/null)
    code=$?
    set -e
    [ "$code" -eq 1 ] || fail "非交互环境应拒绝，实际 $code"
    case "$out" in *--yes*) ;; *) fail '应提示加 --yes' ;; esac

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
      echo 'run `just get-test-binary` to fetch busybox-static for aarch64'
    fi

    step 'local checks passed'
    echo 'next: sudo just kernel-test'

# 真会话里的弹框冒烟：真的把窗口弹出来看一眼（会短暂闪窗，不属于 just test）。
# 存在的意义：offscreen 冒烟只看退出码，看不出「窗口根本没成形」这类毛病
# （ApplicationWindow 忘了 visible: true 就是这样：offscreen 下照样过）。
dialog-smoke:
    #!/usr/bin/env bash
    set -euo pipefail
    step() { printf '\n== %s ==\n' "$*"; }
    fail() { printf '!! %s\n' "$*" >&2; exit 1; }

    QML_RUNTIME=/usr/lib/qt6/bin/qml
    [ -x "$QML_RUNTIME" ] || { echo "没装 Qt6 的 qml 运行时（$QML_RUNTIME），跳过"; exit 0; }
    [ -n "${DISPLAY:-}" ] || { echo '没有 DISPLAY，跳过'; exit 0; }
    command -v xdotool > /dev/null \
      || { echo '没装 xdotool（驱动不了窗口），跳过'; exit 0; }

    step '真弹窗冒烟：窗口得真的显示出来，回车 = 运行（退出码 10）'
    "$QML_RUNTIME" "$PWD/data/dialog.qml" -- \
      --title 'aosc-exec-guard 冒烟测试' --text '冒烟测试：按回车（= 运行）或 Esc（= 不运行）。' \
      --checkbox '不再询问' --ok '运行' --cancel '不运行' > /tmp/aosc-qml-smoke.log 2>&1 &
    pid=$!
    win=
    for _ in $(seq 1 20); do
      win=$(xdotool search --pid "$pid" 2>/dev/null | head -1 || true)
      [ -n "$win" ] && break
      sleep 0.25
    done
    if [ -z "$win" ]; then
      kill "$pid" 2>/dev/null || true
      echo 'qml 日志：'; cat /tmp/aosc-qml-smoke.log
      fail 'QML 跑着，但 X 上没有窗口（忘了 visible: true？）'
    fi
    echo "=> 窗口出来了（hwnd $win）"
    w=$(xdotool getwindowgeometry --shell "$win" | sed -n 's/^WIDTH=//p')
    h=$(xdotool getwindowgeometry --shell "$win" | sed -n 's/^HEIGHT=//p')
    echo "   尺寸 ${w}x${h}（HiDPI 下是物理像素，逻辑尺寸见 dialog.qml 的 width/height）"
    xdotool windowactivate --sync "$win" 2>/dev/null || true
    xdotool key --clearmodifiers Return
    set +e
    wait "$pid"
    code=$?
    set -e
    [ "$code" -eq 10 ] || fail "回车后应退 10（运行），实际 $code"
    echo '=> 真弹窗冒烟通过'

# 内核端到端测试（需要 root）：注册/优先级/chroot/qemu 转发/自动清理
kernel-test:
    #!/usr/bin/env bash
    set -euo pipefail

    # 不在 root 下构建（root 的 rustup 可能没有默认 toolchain）：先以用户身份跑 just build。
    if [ "$(id -u)" -ne 0 ]; then
      echo '需要 root：sudo just kernel-test' >&2
      exit 1
    fi

    GUARD=$PWD/target/static/aosc-exec-guard
    [ -x "$GUARD" ] || { echo "找不到 $GUARD；先跑 just build" >&2; exit 1; }

    TMP=tests/tmp
    mkdir -p "$TMP/home"
    [ -f "$TMP/aarch64.elf" ] || { echo "找不到测试文件 $TMP/aarch64.elf；先跑 just test" >&2; exit 1; }
    [ -x "$TMP/aarch64.elf" ] || chmod +x "$TMP/aarch64.elf"

    BM=/proc/sys/fs/binfmt_misc
    ENTRY=$BM/aosc-exec-guard-aarch64
    QEMU_ENTRY=$BM/qemu-aarch64

    step() { printf '\n== %s ==\n' "$*"; }
    fail() { echo "FAIL: $*" >&2; exit 1; }

    # Read a (procfs) file with shell builtins only: works even when exec is broken.
    show_file() { local line; while IFS= read -r line; do printf '  %s\n' "$line"; done < "$1"; }

    # Run a foreign binary the way a user would: text mode so that no dialog ever
    # blocks this run, a private HOME so no saved “不再询问” answer leaks in, and
    # AOSC_EXEC_GUARD_QEMU=never so the guard explains instead of asking/running.
    run_target() {
      env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_CONFIG_HOME AOSC_EXEC_GUARD_NO_DIALOG=1 \
        AOSC_EXEC_GUARD_QEMU=never AOSC_EXEC_GUARD_LANG=zh_CN HOME="$PWD/$TMP/home" "$@"
    }

    QEMU_WAS_ENABLED=no
    if [ -e "$QEMU_ENTRY" ]; then
      qemu_first_line=''
      read -r qemu_first_line < "$QEMU_ENTRY" || true
      [ "$qemu_first_line" = enabled ] && QEMU_WAS_ENABLED=yes
    fi

    cleanup() {
      set +e
      if [ -n "${CHR:-}" ]; then
        umount "$CHR/proc" 2>/dev/null
        rm -rf "$CHR"
      fi
      [ -n "${CHR3:-}" ] && rm -rf "$CHR3"
      [ -e "$ENTRY" ] && echo -1 > "$ENTRY"
      if [ -e "$QEMU_ENTRY" ]; then
        if [ "$QEMU_WAS_ENABLED" = yes ]; then echo 1 > "$QEMU_ENTRY"; else echo 0 > "$QEMU_ENTRY"; fi
      fi
      set -e
    }
    trap cleanup EXIT

    # An entry matching the *host* architecture would hijack the interpreter
    # itself and send the kernel into an exec recursion loop (see README). This
    # test registers an AArch64 entry, so refuse to run on an AArch64 host.
    case "$(uname -m)" in
      aarch64|arm64) fail '本机就是 aarch64：这个实验不能在它上面做';;
    esac

    step 'binfmt_misc 状态'
    [ -e "$BM/status" ] || mount -t binfmt_misc binfmt_misc "$BM"
    show_file "$BM/status"

    step "注册 guard 条目（interpreter=$GUARD）"
    [ -e "$ENTRY" ] && echo -1 > "$ENTRY"
    line=$(grep -F ':aosc-exec-guard-aarch64:' data/binfmt.d/zz-aosc-exec-guard.conf.in)
    line=${line//\/usr\/bin\/aosc-exec-guard/$GUARD}
    printf '%s\n' "$line" > "$BM/register"
    show_file "$ENTRY"

    # Smoke test: the host must still be able to run its own binaries. If this
    # breaks, the entry is wrong — abort and let the trap clean up.
    if ! /usr/bin/true 2>/dev/null; then
      fail '注册后本机程序无法执行，已中止'
    fi
    echo '  （本机程序执行正常）'

    step '优先级实验：guard 与 qemu-aarch64 同时可用时，谁先匹配'
    set +e
    out=$(run_target "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf '%s\nexit=%s\n' "$out" "$code"
    case "$out" in
      *'无法运行'*)   echo '=> guard 先匹配' ;;
      *qemu*)         echo '=> qemu-aarch64 先匹配' ;;
      *'Exec format error'*) echo '=> 未被任何条目匹配（注册失败？）' ;;
      *)              echo '=> 无法判断，原样输出见上' ;;
    esac

    step '暂时禁用 qemu-aarch64，验证 guard 的完整链路'
    [ -e "$QEMU_ENTRY" ] && echo 0 > "$QEMU_ENTRY"
    set +e
    out=$(run_target "$TMP/aarch64.elf" 2>&1)
    code=$?
    set -e
    printf '%s\nexit=%s\n' "$out" "$code"
    [ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
    case "$out" in *aarch64*) ;; *) fail 'message should mention aarch64' ;; esac

    if [ -x "$TMP/busybox-aarch64" ]; then
      step '真实 aarch64 二进制（busybox）走 guard'
      set +e
      out=$(run_target "$TMP/busybox-aarch64" 2>&1)
      code=$?
      set -e
      printf '%s\nexit=%s\n' "$out" "$code"
      [ "$code" -eq 126 ] || fail "exit code should be 126, got $code"
    fi

    step 'chroot：宿主条目照样命中；静态 guard + F —— 空 rootfs 里零拷贝直接能解释'
    CHR=$PWD/$TMP/chroot
    rm -rf "${CHR:?}"
    mkdir -p "$CHR/proc"
    cp "$TMP/aarch64.elf" "$CHR/prog"
    chmod +x "$CHR/prog"

    # 条目带 F（fix binary）：解释器文件在注册时就打开，exec 时直接用宿主机上
    # 那一份；guard 是全静态的，所以 rootfs 里什么都不用放（动态解释器会以
    # ENOENT 收场——那正是把动态构建删掉的原因）。
    set +e
    out=$(env AOSC_EXEC_GUARD_NO_DIALOG=1 AOSC_EXEC_GUARD_LANG=zh_CN chroot "$CHR" /prog 2>&1)
    code=$?
    set -e
    printf '%s\nexit=%s\n' "$out" "$code"
    [ "$code" -eq 126 ] || fail "空 rootfs 里静态 guard 应该解释并退 126，实际 $code"
    case "$out" in *aarch64*) ;; *) fail '空 rootfs 里的解释也该提到 aarch64' ;; esac
    case "$out" in *--handover*) ;; *) fail '解释里应给出 --handover 这条出路' ;; esac

    # 挂了 /proc 后，guard 能看出自己在 chroot 里（提示相应改变）
    mount -t proc proc "$CHR/proc"
    set +e
    out=$(env AOSC_EXEC_GUARD_NO_DIALOG=1 AOSC_EXEC_GUARD_LANG=zh_CN chroot "$CHR" /prog 2>&1)
    code=$?
    set -e
    umount "$CHR/proc"
    printf '%s\nexit=%s\n' "$out" "$code"
    case "$out" in *chroot*) echo '=> guard 认出了 chroot，提示相应改变' ;; *) fail 'chroot 里的提示应该提到 chroot' ;; esac

    # 看不到注册表时就借宿主机的注册表（/proc/1/root）——F 条目下内核用的就是宿主机那份
    if [ "$QEMU_WAS_ENABLED" = yes ] && [ -x /usr/bin/qemu-aarch64-static ]; then
      step 'chroot：看不到注册表时借宿主机 qemu 条目转发（/proc/1/root）'
      [ -e "$QEMU_ENTRY" ] && echo 1 > "$QEMU_ENTRY"
      mount -t proc proc "$CHR/proc"
      set +e
      out=$(env AOSC_EXEC_GUARD_NO_DIALOG=1 AOSC_EXEC_GUARD_LANG=zh_CN chroot "$CHR" /prog 2>&1)
      code=$?
      set -e
      printf '%s\nexit=%s\n' "$out" "$code"
      [ "$code" -ne 126 ] || fail 'chroot 里应借宿主机 qemu 条目转发，不该只解释（126）'
      case "$out" in *qemu*) ;; *) fail '应当看到 qemu 的输出（它会对假 ELF 报错）' ;; esac
      if [ -x "$TMP/busybox-aarch64" ]; then
        cp "$TMP/busybox-aarch64" "$CHR/busybox"
        set +e
        out=$(env AOSC_EXEC_GUARD_NO_DIALOG=1 AOSC_EXEC_GUARD_LANG=zh_CN chroot "$CHR" /busybox uname -m 2>&1)
        code=$?
        set -e
        printf '%s\nexit=%s\n' "$out" "$code"
        [ "$code" -eq 0 ] || fail "busybox 应该经宿主条目的模拟器跑起来，实际 $code"
        case "$out" in *aarch64*) ;; *) fail 'busybox 的 uname -m 应输出 aarch64' ;; esac
        rm -f "$CHR/busybox"
      fi
      umount "$CHR/proc"
      [ -e "$QEMU_ENTRY" ] && echo 0 > "$QEMU_ENTRY"
    fi
    rm -rf "${CHR:?}"

    if [ -x "$TMP/busybox-aarch64" ] && [ "$QEMU_WAS_ENABLED" = yes ]; then
      step 'chroot（没挂 /proc）：guard 转不了，提示 --handover 这条出路'
      [ -e "$QEMU_ENTRY" ] && echo 1 > "$QEMU_ENTRY"
      CHR3=$PWD/$TMP/chroot-handover
      rm -rf "${CHR3:?}"
      mkdir -p "$CHR3"
      cp "$TMP/aarch64.elf" "$CHR3/prog"
      chmod +x "$CHR3/prog"
      cp "$TMP/busybox-aarch64" "$CHR3/busybox"
      set +e
      out=$(env AOSC_EXEC_GUARD_NO_DIALOG=1 AOSC_EXEC_GUARD_LANG=zh_CN chroot "$CHR3" /prog 2>&1)
      code=$?
      set -e
      printf '%s\nexit=%s\n' "$out" "$code"
      [ "$code" -eq 126 ] || fail "没挂 /proc 的 chroot 里 guard 只能解释，实际 $code"
      case "$out" in *--handover*) ;; *) fail '解释里应给出 --handover 这条出路' ;; esac

      step '--handover：guard 退出，内核的 qemu 条目接管（同一个 chroot 立刻能跑）'
      # 配置目录指到临时目录：不动宿主机的 /usr/lib/binfmt.d（本机也没装过）
      AOSC_EXEC_GUARD_CONF_DIRS="$PWD/$TMP/confdir" "$GUARD" --handover --yes \
        || fail '--handover 失败'
      [ ! -e "$ENTRY" ] || fail '--handover 之后 guard 条目应该消失'
      set +e
      out=$(chroot "$CHR3" /busybox uname -m 2>&1)
      code=$?
      set -e
      printf '%s\nexit=%s\n' "$out" "$code"
      [ "$code" -eq 0 ] || fail "让位后内核的 qemu F 条目应直接把 busybox 跑起来，实际 $code"
      case "$out" in *aarch64*) ;; *) fail 'busybox 的 uname -m 应输出 aarch64' ;; esac
      rm -rf "${CHR3:?}"

      # 把手动注册的条目加回来，后面的步骤还要用
      printf '%s\n' "$line" > "$BM/register"
    fi

    if [ -x "$TMP/busybox-aarch64" ] && [ "$QEMU_WAS_ENABLED" = yes ]; then
      step 'guard 转发给 qemu：AOSC_EXEC_GUARD_QEMU=always 时 busybox 真的跑起来'
      # 转发要求 qemu 条目处于启用状态（上一步把它禁用过）
      [ -e "$QEMU_ENTRY" ] && echo 1 > "$QEMU_ENTRY"
      set +e
      out=$(env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_CONFIG_HOME AOSC_EXEC_GUARD_QEMU=always \
        HOME="$PWD/$TMP/home" "$TMP/busybox-aarch64" uname -m 2>&1)
      code=$?
      set -e
      printf '%s\nexit=%s\n' "$out" "$code"
      [ "$code" -eq 0 ] || fail "busybox 应经 guard 交给 qemu 运行，实际退出码 $code"
      case "$out" in *aarch64*) ;; *) fail 'busybox 的 uname -m 应输出 aarch64' ;; esac
    fi

    step '恢复 qemu-aarch64，并确认模拟器行为恢复'
    if [ "$QEMU_WAS_ENABLED" = yes ]; then echo 1 > "$QEMU_ENTRY"; fi
    [ -e "$ENTRY" ] && echo -1 > "$ENTRY"
    if [ -x "$TMP/busybox-aarch64" ] && [ "$QEMU_WAS_ENABLED" = yes ]; then
      set +e
      out=$(run_target "$TMP/busybox-aarch64" true 2>&1)
      code=$?
      set -e
      printf '%s\nexit=%s\n' "$out" "$code"
      if [ "$code" -eq 0 ]; then
        echo '=> qemu 模拟运行恢复正常'
      else
        fail 'qemu 未恢复，请检查 binfmt 条目'
      fi
    fi

    step '内核端到端测试通过'

# 真实安装路径测试（需要 root）：把 conf 装进 /usr/lib/binfmt.d，交给 systemd-binfmt
systemd-install-test:
    #!/usr/bin/env bash
    set -euo pipefail

    if [ "$(id -u)" -ne 0 ]; then
      echo '需要 root：sudo just systemd-install-test' >&2
      exit 1
    fi
    case "$(uname -m)" in
      aarch64|arm64) echo '本机是 aarch64：跳过（条目会劫持解释器自身）' >&2; exit 1;;
    esac

    CONF_DST=/usr/lib/binfmt.d/zz-aosc-exec-guard.conf
    GUARD=$PWD/target/static/aosc-exec-guard
    [ -x "$GUARD" ] || { echo "找不到 $GUARD；先跑 just build" >&2; exit 1; }
    BM=/proc/sys/fs/binfmt_misc
    ENTRY=$BM/aosc-exec-guard-aarch64
    TMP=tests/tmp
    [ -f "$TMP/aarch64.elf" ] || { echo "先跑 just test 生成 $TMP/aarch64.elf" >&2; exit 1; }
    [ -x "$TMP/aarch64.elf" ] || chmod +x "$TMP/aarch64.elf"

    step() { printf '\n== %s ==\n' "$*"; }

    restart_binfmt() {
      # restart 的 stop 阶段会注销所有条目；start 若被 systemd 的“开始频率限制”挡下
      # （测试里短时间内会 restart 好几次），条目就全没了。先清掉计数再重启。
      systemctl reset-failed systemd-binfmt.service 2>/dev/null || true
      systemctl restart systemd-binfmt.service
    }

    show_entry() {
      if [ -e "$BM/$1" ]; then
        local line
        echo "-- $1"
        while IFS= read -r line; do printf '   %s\n' "$line"; done < "$BM/$1"
      else
        echo "-- $1: 不存在"
      fi
    }

    probe() { # run the fake aarch64 file; sets $probe_result to guard/qemu/none
      set +e
      local out code
      out=$(env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_CONFIG_HOME AOSC_EXEC_GUARD_NO_DIALOG=1 \
        AOSC_EXEC_GUARD_QEMU=never AOSC_EXEC_GUARD_LANG=zh_CN HOME="$PWD/$TMP/home" "$TMP/aarch64.elf" 2>&1)
      code=$?
      set -e
      printf '%s\nexit=%s\n' "$out" "$code"
      case "$out" in
        *'无法运行'*)  probe_result=guard; echo '=> guard 先匹配' ;;
        *qemu*)        probe_result=qemu;  echo '=> qemu-aarch64 先匹配' ;;
        *)             probe_result=none;  echo '=> 未被任何条目匹配' ;;
      esac
    }

    cleanup() {
      set +e
      update-alternatives --remove emu-aarch64.conf /usr/lib/binfmt.alternatives/aosc-exec-guard-aarch64.conf > /dev/null 2>&1
      rm -f "$CONF_DST" "$CONF_DST.disabled"
      rm -f /usr/lib/binfmt.alternatives/aosc-exec-guard-aarch64.conf
      [ "${created_qemu_alt:-no}" = yes ] && rm -f /usr/lib/binfmt.alternatives/qemu-aarch64.conf
      restart_binfmt
      [ -e "$ENTRY" ] && echo -1 > "$ENTRY"
      set -e
    }
    trap cleanup EXIT

    step '安装 conf（走 scripts/install.sh 生成过滤后的 conf，模拟打包安装）'
    # 安装脚本会按本机家族删掉本机架构的规则（不删的话条目会劫持解释器自身）。
    # F 标志要求解释器文件在注册时就存在（这里指向本仓库构建的二进制）。
    PKG=$(mktemp -d)
    scripts/install.sh --prefix "$PKG" > /dev/null
    sed "s|/usr/bin/aosc-exec-guard|$GUARD|" "$PKG/lib/binfmt.d/zz-aosc-exec-guard.conf" > "$CONF_DST"
    rm -rf "$PKG"
    chmod 644 "$CONF_DST"
    restart_binfmt
    show_entry aosc-exec-guard-aarch64
    # 一个 conf 文件里的多条规则（systemd-binfmt 逐行注册）都应该生效
    for arch in aarch64 arm loongarch64 riscv64 alpha; do
      if [ ! -e "$BM/aosc-exec-guard-$arch" ]; then
        echo "FAIL: 条目 aosc-exec-guard-$arch 未注册" >&2
        exit 1
      fi
    done
    printf '  （一个 conf 文件里的多条规则全部注册：aarch64/arm/loongarch64/riscv64/alpha …）\n'
    # 注册后立刻冒烟：本机必须还能跑原生程序（万一过滤漏了本机架构，这里会 ELOOP）
    if ! /usr/bin/true 2>/dev/null; then
      echo 'FAIL: 注册后本机无法执行程序，立刻用内建命令撤销 guard 条目' >&2
      while IFS= read -r line; do
        case "$line" in
          :aosc-exec-guard-*) ;;
          *) continue ;;
        esac
        name=${line#:}
        name=${name%%:*}
        [ -e "$BM/$name" ] && echo -1 > "$BM/$name"
      done < "$CONF_DST"
      exit 1
    fi
    echo '  （本机程序执行正常）'

    step '运行时优先级：服务刚注册的 guard vs 开机时的 qemu'
    probe
    [ "$probe_result" = guard ] || echo '（注意：本次不是 guard 先匹配）'

    step '模拟干净启动顺序：清空所有条目，让 systemd-binfmt 按文件名排序重放'
    for f in "$BM"/*; do
      name=${f##*/}
      case "$name" in
        register|status) ;;
        *) echo -1 > "$f" 2>/dev/null || true ;;
      esac
    done
    restart_binfmt
    show_entry aosc-exec-guard-aarch64
    show_entry qemu-aarch64
    probe
    # zz- 前缀让 guard 的 conf 排在 qemu-* 之后应用（后应用者优先），
    # 于是干净启动时也由 guard 先接住、再询问用户要不要用仿真器运行。
    if [ "$probe_result" = guard ]; then
      echo '=> 符合预期：zz- 名让 guard 排在 qemu 之后应用，所以 guard 先匹配'
    else
      echo 'FAIL: 干净启动顺序下应当是 guard 先匹配（conf 名以 zz- 开头）' >&2
      exit 1
    fi

    step '--handover：注销条目、停用 conf（真安装路径）'
    "$GUARD" --handover --yes
    if [ -e "$BM/aosc-exec-guard-aarch64" ]; then
      echo 'FAIL: --handover 之后 guard 条目应该消失' >&2
      exit 1
    fi
    if [ ! -e "$CONF_DST.disabled" ]; then
      echo 'FAIL: conf 应改名为 .disabled' >&2
      exit 1
    fi
    probe
    [ "$probe_result" = qemu ] || echo '（注意：让位后本次不是 qemu 先匹配）'

    step '--handover=off：恢复 conf 与条目'
    "$GUARD" --handover=off --yes
    if [ ! -e "$CONF_DST" ] || [ -e "$CONF_DST.disabled" ]; then
      echo 'FAIL: conf 应恢复原名' >&2
      exit 1
    fi
    if [ ! -e "$BM/aosc-exec-guard-aarch64" ]; then
      echo 'FAIL: --handover=off 后 guard 条目应该回来' >&2
      exit 1
    fi
    probe
    [ "$probe_result" = guard ] || echo '（注意：恢复后本次不是 guard 先匹配）'

    step '清理：移除 conf、重放，并确认 guard 条目消失'
    rm -f "$CONF_DST"
    restart_binfmt
    if [ -e "$ENTRY" ]; then
      echo '（重放后条目仍在：systemd-binfmt 不会自动清理不在配置里的条目，需要显式注销）'
      echo -1 > "$ENTRY"
    fi
    if [ -e "$ENTRY" ]; then
      echo 'FAIL: guard 条目未清除' >&2
      exit 1
    fi
    echo '=> guard 条目已移除'

    if [ -x "$TMP/busybox-aarch64" ]; then
      step '最终确认：qemu 仍能模拟运行 aarch64'
      set +e
      out=$(env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_CONFIG_HOME AOSC_EXEC_GUARD_QEMU=never \
        HOME="$PWD/$TMP/home" "$TMP/busybox-aarch64" true 2>&1)
      code=$?
      set -e
      printf '%s\nexit=%s\n' "$out" "$code"
      if [ "$code" -eq 0 ]; then
        echo '=> qemu 正常'
      else
        echo '=> 注意：qemu 未恢复，请检查 binfmt 条目（重启也能恢复）' >&2
      fi
    fi

    step 'alternatives 模式（box64 式）：槽位 emu-aarch64.conf 指向 guard，模拟器从候选 conf 里找'
    ALT_DIR=/usr/lib/binfmt.alternatives
    GUARD_ALT=$ALT_DIR/aosc-exec-guard-aarch64.conf
    QEMU_ALT=$ALT_DIR/qemu-aarch64.conf
    SLOT=/usr/lib/binfmt.d/emu-aarch64.conf
    running=$GUARD   # 工具正在跑，别删自己
    # staging 出 alternatives 布局，把解释器指到本仓库的二进制，再登记槽位
    PKG=$(mktemp -d)
    scripts/install.sh --prefix "$PKG" --alternatives > /dev/null
    install -Dm644 "$PKG/lib/binfmt.alternatives/aosc-exec-guard-aarch64.conf" "$GUARD_ALT"
    sed -i "s|/usr/bin/aosc-exec-guard|$running|" "$GUARD_ALT"
    rm -rf "$PKG"
    update-alternatives --install "$SLOT" emu-aarch64.conf "$GUARD_ALT" 100
    restart_binfmt
    show_entry aosc-exec-guard-aarch64
    [ -L "$SLOT" ] || { echo 'FAIL: 槽位 emu-aarch64.conf 应该是 alternatives 链接' >&2; exit 1; }
    printf '  %s → %s\n' "$SLOT" "$(readlink "$SLOT")"
    # 把 qemu 的条目从内核里拿掉，复现“干净世界”：模拟器包也搬进 alternatives 之后，
    # emu-aarch64.conf 这个槽位只指向 guard，注册表里根本没有 qemu 的条目。
    [ -e "$BM/qemu-aarch64" ] && echo -1 > "$BM/qemu-aarch64"
    probe_entry() { # 从 --debug 行里取 qemu=<谁>（'-' = 没找到模拟器）
      set +e
      found=$(env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_CONFIG_HOME AOSC_EXEC_GUARD_DEBUG=1 \
        AOSC_EXEC_GUARD_NO_DIALOG=1 AOSC_EXEC_GUARD_QEMU=never AOSC_EXEC_GUARD_LANG=zh_CN \
        HOME="$PWD/$TMP/home" "$GUARD" "$TMP/aarch64.elf" 2>&1 |
        sed -n 's/.* qemu=\([^ ]*\) .*/\1/p' | head -1)
      set -e
    }
    probe_entry
    printf '  注册表里没有 qemu、alternatives 里也没有候选：qemu=%s\n' "$found"
    [ "$found" = '-' ] || { echo 'FAIL: 没有候选时不该凭空认出模拟器' >&2; exit 1; }
    if [ -f /usr/lib/binfmt.d/qemu-aarch64.conf ]; then
      # 模拟器包搬进 alternatives 之后的样子：候选 conf 和 guard 的摆在一起
      if [ ! -e "$QEMU_ALT" ]; then
        install -Dm644 /usr/lib/binfmt.d/qemu-aarch64.conf "$QEMU_ALT"
        created_qemu_alt=yes
      fi
      probe_entry
      printf '  把 qemu 的 conf 放进 binfmt.alternatives 之后：qemu=%s\n' "$found"
      [ "$found" = 'qemu-aarch64' ] \
        || { echo 'FAIL: 应该从 binfmt.alternatives 的候选 conf 里认出 qemu-aarch64' >&2; exit 1; }
      echo '=> 槽位归 guard 时，模拟器从 /usr/lib/binfmt.alternatives/ 的候选里找（qemu / box64 / FEX… 一视同仁）'
    else
      echo '（没装 qemu-aarch64-static？跳过候选那半段）'
    fi
    # 还原：撤掉槽位与临时 conf，restart 把 qemu 的条目带回来
    update-alternatives --remove emu-aarch64.conf "$GUARD_ALT" > /dev/null
    rm -f "$GUARD_ALT"
    restart_binfmt
    [ ! -e "$SLOT" ] || { echo 'FAIL: 槽位没被移除' >&2; exit 1; }
    echo '=> alternatives 槽位已清理'

    step 'systemd-binfmt 安装路径测试通过'

# 安装：just install [前缀] [目标架构] [ignore-family]
install prefix="" target_arch="" ignore_family="no":
    #!/usr/bin/env bash
    # 实现都在 scripts/install.sh（打包可以直接调那个脚本，不必依赖 just）：
    #   前缀为空或 /usr → 装进 /usr，重启 systemd-binfmt 并做 exec 冒烟测试
    #   其它前缀 → 只写文件（打包/staging 用，不碰内核）
    set -euo pipefail
    args=()
    if [ -n "$1" ]; then args+=(--prefix "$1"); fi
    if [ -n "$2" ]; then args+=(--host-arch "$2"); fi
    if [ "$3" = yes ]; then args+=(--ignore-family yes); fi
    exec scripts/install.sh "${args[@]}"

# 卸载：just uninstall [前缀]
uninstall prefix="":
    #!/usr/bin/env bash
    # 实现也在 scripts/install.sh。
    set -euo pipefail
    if [ -n "$1" ]; then
      exec scripts/install.sh --uninstall --prefix "$1"
    else
      exec scripts/install.sh --uninstall
    fi

# 下载真实的 aarch64 静态二进制（可选；just test / kernel-test 会用它）
get-test-binary:
    #!/usr/bin/env bash
    set -euo pipefail

    TMP=tests/tmp
    mkdir -p "$TMP"

    BASE="${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}/latest-stable/main/aarch64"

    echo "== fetching package index: $BASE/"
    index=$(curl -fsS "$BASE/")
    file=$(printf '%s' "$index" | grep -o 'busybox-static-[0-9][^"<]*\.apk' | head -n1 || true)
    [ -n "$file" ] || { echo 'no busybox-static found in index' >&2; exit 1; }

    echo "== downloading $file"
    curl -fsS -o "$TMP/busybox.apk" "$BASE/$file"

    echo '== extracting bin/busybox.static'
    tar -xzf "$TMP/busybox.apk" -C "$TMP" bin/busybox.static
    mv -f "$TMP/bin/busybox.static" "$TMP/busybox-aarch64"
    rmdir "$TMP/bin" 2>/dev/null || true
    rm -f "$TMP/busybox.apk"

    file "$TMP/busybox-aarch64" || true
    echo "== saved to $TMP/busybox-aarch64"
