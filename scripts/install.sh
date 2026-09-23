#!/usr/bin/env bash
# aosc-exec-guard 安装器：装二进制 + 生成按本机架构过滤过的 binfmt conf。
#
#   scripts/install.sh                        # 装进 /usr，重启 systemd-binfmt 并做 exec 冒烟测试
#   scripts/install.sh --prefix /tmp/pkg      # 只写文件（打包/staging 用，不碰内核）
#   scripts/install.sh --host-arch aarch64    # 交叉打包时指定目标架构（默认 uname -m）
#   scripts/install.sh --ignore-family yes    # 同家族但本机跑不了的目标（如 riscv64 上的 riscv32）
#   scripts/install.sh --alternatives         # box64 式布局：conf 进 binfmt.alternatives，靠 alternatives 槽位生效
#   scripts/install.sh --priority 100         # alternatives 模式下的优先级（默认 100）
#   scripts/install.sh --uninstall            # 卸载（--prefix 同理；两种模式都会清）
#
# 过滤逻辑和 qemu 的 scripts/qemu-binfmt-conf.sh 一致：把与本机**家族**相同的规则整族
# 删掉（amd64 上删 i386+x86_64、aarch64 上删 arm+aarch64、mips64 上删 mips 一族…）。
# 卸不掉本机架构的规则会让条目劫持解释器自身：内核解释器递归到 ELOOP，全系统起不了新程序。
# 装到 /usr 时脚本还会用 /usr/bin/true 做冒烟测试，失败就立刻撤销（只用内建命令，因为那时
# exec 已经坏了）。
#
# 两种装法（默认是第一种）：
#  1. 默认：整份过滤后的 conf 直接装成 `lib/binfmt.d/zz-aosc-exec-guard.conf`。适用于
#     手动安装（模拟器包也是直接把 conf 放 /usr/lib/binfmt.d/ 的那套系统）：按文件名排序
#     应用，`zz-` 排在 `qemu-*.conf` 之后 → guard 先接住，再由它决定询不询问。
#  2. --alternatives：跟 app-emulation/box64 包一个做法——每条规则一个按架构的 conf，装在
#     `lib/binfmt.alternatives/`，用 update-alternatives 把槽位 `lib/binfmt.d/emu-<arch>.conf`
#     指到它（优先级见 --priority）。适用于包管理场景：同一个架构有好几个包能解释（qemu /
#     box64 / FEX / latx…），谁生效由 alternatives 优先级定，不再看文件名。
set -euo pipefail
cd "$(dirname "$0")/.."

CONF_SRC=$PWD/data/binfmt.d/zz-aosc-exec-guard.conf.in
BIN_SRC=$PWD/target/static/aosc-exec-guard

PREFIX=/usr
PREFIX_GIVEN=no
TARGET_ARCH=${HOST_ARCH:-}   # 空 = 用 uname -m
IGNORE_FAMILY=no
ALTERNATIVES=no
PRIORITY=100
UNINSTALL=no

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX=$2; PREFIX_GIVEN=yes; shift 2 ;;
    --host-arch) TARGET_ARCH=$2; shift 2 ;;
    --ignore-family) IGNORE_FAMILY=$2; shift 2 ;;
    --alternatives) ALTERNATIVES=yes; shift ;;
    --priority) PRIORITY=$2; shift 2 ;;
    --uninstall) UNINSTALL=yes; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "未知参数：$1" >&2; usage >&2; exit 2 ;;
  esac
done

BIN_DST=$PREFIX/bin/aosc-exec-guard
SLOT_DIR=$PREFIX/lib/binfmt.d
CONF_DST=$SLOT_DIR/zz-aosc-exec-guard.conf
ALT_DIR=$PREFIX/lib/binfmt.alternatives
ALT_REF=$PREFIX/share/aosc-exec-guard/alternatives
QML_SRC=$PWD/data/dialog.qml
QML_DST=$PREFIX/share/aosc-exec-guard/dialog.qml
BM=/proc/sys/fs/binfmt_misc

# uname -m / HOST_ARCH 的写法 → 我们的条目名（同 qemu 的 qemu_normalize）。
normalize_arch() {
  case "$1" in
    i[3-6]86) echo i386 ;;
    amd64) echo x86_64 ;;
    arm64) echo aarch64 ;;
    armel|armhf|armv[4-9]*l) echo arm ;;
    armv[4-9]*b) echo armeb ;;
    powerpc) echo ppc ;;
    ppc64el) echo ppc64le ;;
    *) echo "$1" ;;
  esac
}

# 条目名 → 家族（抄自 qemu 的 qemu-binfmt-conf.sh；同家族的目标本机自己能原生跑）。
entry_family() {
  case "$1" in
    i386|x86_64) echo i386 ;;
    arm|aarch64) echo arm ;;
    armeb|aarch64_be) echo armeb ;;
    mips|mipsel|mipsn32|mipsn32el|mips64|mips64el) echo mips ;;
    ppc|ppc64) echo ppc ;;
    ppc64le) echo ppcle ;;
    sh4|sh4eb) echo sh4 ;;
    sparc|sparc32plus|sparc64) echo sparc ;;
    riscv32|riscv64) echo riscv ;;
    loongarch64) echo loongarch ;;
    alpha|m68k|microblaze|microblazeel|or1k|hppa|xtensa|xtensaeb|hexagon) echo "$1" ;;
    *) echo "" ;;
  esac
}

LOCAL_ARCH=$(normalize_arch "$(uname -m)")
HOST_ARCH=$(normalize_arch "${TARGET_ARCH:-$(uname -m)}")
HOST_FAMILY=$(entry_family "$HOST_ARCH")
if [ -z "$HOST_FAMILY" ]; then
  echo "不认识的架构 $HOST_ARCH：不知道它属于哪个家族，没法安全地过滤规则。" >&2
  echo "请用 --host-arch <x86_64|aarch64|riscv64|mips64|…> 指定。" >&2
  exit 1
fi

# 从规则行里取出名字/magic/mask：纯 bash，exec 坏掉时也还能用。
rule_fields() { # $1=规则行 → RULE_NAME / RULE_MAGIC / RULE_MASK
  RULE_NAME=${1#:}
  RULE_NAME=${RULE_NAME%%:*}
  local rest=${1#*:M::}
  RULE_MAGIC=${rest%%:*}
  RULE_MASK=${rest#*:}
  RULE_MASK=${RULE_MASK%%:*}
}

# "\x7fELF\x02…" → 空格分隔的十进制字节
bytes_of() {
  local s=$1 out=()
  while [ -n "$s" ]; do
    if [ "${s:0:2}" = '\x' ]; then
      out+=("$((16#${s:2:2}))")
      s=${s:4}
    else
      out+=("$(printf '%d' "'${s:0:1}")")
      s=${s:1}
    fi
  done
  printf '%s' "${out[*]}"
}

# 规则会不会命中这个文件？（和内核同一套 magic/mask 比较）
rule_matches_file() { # $1=规则行 $2=文件
  local -a magic mask file
  rule_fields "$1"
  read -r -a magic <<<"$(bytes_of "$RULE_MAGIC")"
  read -r -a mask <<<"$(bytes_of "$RULE_MASK")"
  read -r -a file <<<"$(od -An -tu1 -v -N "${#magic[@]}" "$2")"
  [ "${#file[@]}" -eq "${#magic[@]}" ] || return 1
  local i
  for ((i = 0; i < ${#magic[@]}; i++)); do
    if (((file[i] & mask[i]) != (magic[i] & mask[i]))); then
      return 1
    fi
  done
  return 0
}

# 过滤：本机家族的规则整族删掉（--ignore-family=yes 时只删完全同名的那个）。
generate_conf() {
  local line arch family
  while IFS= read -r line; do
    case "$line" in
      :aosc-exec-guard-*) ;;
      *)
        printf '%s\n' "$line"   # 注释、空行原样保留
        continue
        ;;
    esac
    rule_fields "$line"
    arch=${RULE_NAME#aosc-exec-guard-}
    family=$(entry_family "$arch")
    if [ "$family" = "$HOST_FAMILY" ] && { [ "$arch" = "$HOST_ARCH" ] || [ "$IGNORE_FAMILY" != yes ]; }; then
      continue
    fi
    printf '%s\n' "$line"
  done <"$CONF_SRC"
}

# 安装前的自检：过滤后的规则里不能有一条命中"本机架构的 ELF"（这里用 guard 自己当样本，
# 它按定义就是本机架构）。交叉打包时样本不是目标架构，跳过并提醒。
check_conf_against_binary() { # $1=conf $2=ELF
  local line
  while IFS= read -r line; do
    case "$line" in
      :aosc-exec-guard-*) ;;
      *) continue ;;
    esac
    if rule_matches_file "$line" "$2"; then
      rule_fields "$line"
      echo "FAIL: 规则 $RULE_NAME 会命中解释器自身（$2）：装上去内核解释器会递归成 ELOOP。" >&2
      return 1
    fi
  done <"$1"
  return 0
}

# 撤销 guard 条目：exec 坏掉时只能用内建命令（echo 重定向），所以读文件、写 procfs。
undo_entries() { # $1=conf
  local line
  while IFS= read -r line; do
    case "$line" in
      :aosc-exec-guard-*) ;;
      *) continue ;;
    esac
    rule_fields "$line"
    if [ -e "$BM/$RULE_NAME" ]; then
      echo -1 >"$BM/$RULE_NAME"
      printf '  已注销 %s\n' "$RULE_NAME"
    fi
  done <"$1"
}

# 我们装下的 conf：普通模式一份，alternatives 模式按架构多份。
our_confs() {
  [ -e "$CONF_DST" ] && printf '%s\n' "$CONF_DST"
  alt_confs | cut -f2
  return 0
}

# 每一条规则一个文件：alternatives 的槽位一次只能指向一个 conf，所以按架构拆开
# （app-emulation/box64 也是这么干的：box64.conf 管 x86_64 槽位、box32.conf 管 i386）。
split_conf() { # $1=过滤后的 conf  $2=目标目录
  mkdir -p "$2"
  awk -v dir="$2" -F: '
    /^:aosc-exec-guard-/ {
      arch = $2
      sub(/^aosc-exec-guard-/, "", arch)
      print > (dir "/aosc-exec-guard-" arch ".conf")
    }' "$1"
}

# alternatives 模式装在 ALTERNATIVES 目录里的 conf：输出 架构<TAB>conf 路径
alt_confs() {
  local conf arch
  for conf in "$ALT_DIR"/aosc-exec-guard-*.conf; do
    [ -e "$conf" ] || continue
    arch=$(basename "$conf" .conf)
    printf '%s\t%s\n' "${arch#aosc-exec-guard-}" "$conf"
  done
  return 0
}

# 登记槽位：/usr/lib/binfmt.d/emu-<arch>.conf → 我们的 conf。
install_alternatives() {
  local arch conf
  while IFS=$'\t' read -r arch conf; do
    update-alternatives --install "$SLOT_DIR/emu-$arch.conf" "emu-$arch.conf" "$conf" "$PRIORITY"
  done < <(alt_confs)
}

# 注销我们自己登记过的槽位（没登记过就当没事）。
remove_alternatives() {
  command -v update-alternatives > /dev/null || return 0
  local arch conf
  while IFS=$'\t' read -r arch conf; do
    update-alternatives --remove "emu-$arch.conf" "$conf" > /dev/null 2>&1 || true
  done < <(alt_confs)
}

# 给打包用的 alternatives 声明（abbs 的 autobuild/alternatives 直接抄这几行）。
# 路径写的是**装好之后**的位置（/usr）——staging 前缀只是打包时的中间目录，
# 包解开后 systemd-binfmt 读的是 /usr/lib/binfmt.d/ 与 /etc/binfmt.d/。
write_alt_ref() {
  local arch conf
  {
    printf '# aosc-exec-guard：alternatives 声明（app-emulation/box64 同款做法）。\n'
    printf '# 把下面几行放进 abbs 的 autobuild/alternatives；conf 装到\n'
    printf '# /usr/lib/binfmt.alternatives/（本仓库 --prefix 模式写在 <前缀>/lib/…）。\n'
    while IFS=$'\t' read -r arch conf; do
      printf 'alternative /usr/lib/binfmt.d/emu-%s.conf /usr/lib/binfmt.alternatives/aosc-exec-guard-%s.conf %s\n' \
        "$arch" "$arch" "$PRIORITY"
    done < <(alt_confs)
  } >"$ALT_REF"
}

# 提醒：alternatives 的槽位可能被别的普通 conf 压过——本机还没把模拟器包搬进
# alternatives 时就是这样（`emu-<arch>.conf`(e) 排在 `qemu-<arch>.conf`(q) 前面，
# 后者后注册、优先命中，guard 的条目在注册表里躺着但不生效）。
check_slots_not_shadowed() {
  local arch conf rule magic mask other other_name line
  while IFS=$'\t' read -r arch conf; do
    while IFS= read -r line; do
      case "$line" in
        :aosc-exec-guard-*) rule=$line; break ;;
      esac
    done <"$conf"
    [ -n "${rule:-}" ] || continue
    rule_fields "$rule"
    magic=$RULE_MAGIC mask=$RULE_MASK
    for other in "$SLOT_DIR"/*.conf; do
      [ -e "$other" ] || continue
      other_name=$(basename "$other")
      [ "$other_name" = "emu-$arch.conf" ] && continue
      # 名字排在槽位之后 + 里面有同一套 magic/mask = 它会后注册、把我们压过去
      if [[ "$other_name" > "emu-$arch.conf" ]] && grep -qF -- "$magic" "$other" \
        && grep -qF -- "$mask" "$other"; then
        printf '提示：%s 的规则和 emu-%s.conf 一样，但按文件名排序在它之后注册 → 现在生效的是前者，guard 接不到。\n' \
          "$other" "$arch" >&2
        printf '      要让 guard 真的接管，得让模拟器包也走 alternatives（同一个 emu-%s.conf 槽位）；\n' "$arch" >&2
        printf '      在那之前可以改用默认模式（不带 --alternatives），它靠 zz- 前缀排在 qemu-* 之后。\n' >&2
      fi
    done
  done < <(alt_confs)
}

restart_binfmt() {
  # 先清掉 systemd 的“开始频率限制”：短时间内多次 restart 会被限流，
  # 而 restart 的 stop 阶段已经把条目全注销了，start 再失败就什么都不剩。
  systemctl reset-failed systemd-binfmt.service 2>/dev/null || true
  systemctl restart systemd-binfmt.service 2>/dev/null ||
    echo '（systemctl 重启 systemd-binfmt 没成功：不是 systemd 系统？）' >&2
}

if [ "$UNINSTALL" = yes ]; then
  # 先记下要注销的条目（conf 删掉后就看不到了）
  undo_list=$(mktemp)
  while IFS= read -r conf; do
    cat "$conf" >>"$undo_list"
  done < <(our_confs)
  if [ "$PREFIX_GIVEN" = no ]; then
    remove_alternatives
  fi
  rm -f "$BIN_DST" "$CONF_DST" "$CONF_DST.disabled" "$QML_DST" "$ALT_REF"
  rm -f "$ALT_DIR"/aosc-exec-guard-*.conf
  rmdir "$ALT_DIR" 2>/dev/null || true
  if [ "$PREFIX_GIVEN" = no ]; then
    restart_binfmt
    undo_entries "$undo_list"   # 保险：万一服务没把条目注销掉
  fi
  rm -f "$undo_list"
  echo "已卸载 $BIN_DST，以及 $CONF_DST 与 alternatives 里的 guard conf"
  exit 0
fi

[ -x "$BIN_SRC" ] || {
  echo "找不到 $BIN_SRC；先 just build（静态构建，见 README 的“构建”）。" >&2
  exit 1
}

tmp_conf=$(mktemp)
trap 'rm -f "$tmp_conf"' EXIT
generate_conf >"$tmp_conf"

if [ "$HOST_ARCH" = "$LOCAL_ARCH" ]; then
  check_conf_against_binary "$tmp_conf" "$BIN_SRC" || exit 1
else
  echo "注意：--host-arch=$HOST_ARCH 与本机（$LOCAL_ARCH）不同，跳过\"规则命中解释器自身\"的本地检查。" >&2
fi

install -Dm755 "$BIN_SRC" "$BIN_DST"
# 自带的 Kirigami 弹框（KDE 会话里 guard 优先用它；没装 Qt6/Kirigami 就往下退）
install -Dm644 "$QML_SRC" "$QML_DST"

if [ "$ALTERNATIVES" = yes ]; then
  # box64 式：每条规则一个 conf、进 binfmt.alternatives，槽位由 update-alternatives 指过来。
  # 换装法时把上一种的残留清掉（普通 conf；真装时连之前登记过的槽位一起）。
  rm -f "$CONF_DST" "$CONF_DST.disabled"
  if [ "$PREFIX_GIVEN" = no ]; then
    remove_alternatives
  fi
  split_conf "$tmp_conf" "$ALT_DIR"
  chmod 644 "$ALT_DIR"/aosc-exec-guard-*.conf
  write_alt_ref
  printf '已写入 %s\n已写入 %s/（%s 个按架构的 conf，已去掉 %s 家族的规则）\n已写入 %s\n' \
    "$BIN_DST" "$ALT_DIR" "$(alt_confs | wc -l)" "$HOST_FAMILY" "$QML_DST"
  printf '  alternatives 声明在 %s（优先级 %s；abbs 的 autobuild/alternatives 抄它）\n' \
    "$ALT_REF" "$PRIORITY"
else
  install -Dm644 "$tmp_conf" "$CONF_DST"
  # 装/重装即清除上一轮 --handover 留下的停用标记（重新启用 guard）
  rm -f "$CONF_DST.disabled"
  printf '已写入 %s\n已写入 %s（%s 条规则，已去掉 %s 家族的规则）\n已写入 %s\n' \
    "$BIN_DST" "$CONF_DST" \
    "$(grep -c '^:' "$CONF_DST")" "$HOST_FAMILY" "$QML_DST"
fi

if [ "$PREFIX_GIVEN" = yes ]; then
  echo '（--prefix 模式：没有碰内核；装到目标系统时请在那里执行 scripts/install.sh）'
  exit 0
fi

if [ "$ALTERNATIVES" = yes ]; then
  install_alternatives   # 登记槽位，update-alternatives 自己会建/换链接
fi

restart_binfmt
if /usr/bin/true 2>/dev/null; then
  # 本机能执行了，再确认条目真的注册进了内核：
  # systemd-binfmt 重启失败（比如被限流）时，条目会全部消失。
  if ls "$BM"/aosc-exec-guard-* >/dev/null 2>&1; then
    echo '  （本机程序执行正常）'
    if [ "$ALTERNATIVES" = yes ]; then
      echo "安装完成：$(alt_confs | wc -l) 个 alternatives 槽位已登记（优先级 $PRIORITY）。"
      check_slots_not_shadowed
    else
      echo "安装完成：$(grep -c '^:' "$CONF_DST") 条规则已生效。"
    fi
  else
    echo 'FAIL: conf 已装好，但内核里没有 aosc-exec-guard 条目（systemd-binfmt 没应用配置）。' >&2
    echo '      看看 systemctl status systemd-binfmt.service；conf 本身没回滚，重启后会生效。' >&2
    exit 1
  fi
else
  # 到这儿说明有条规则命中了本机架构：只能靠内建命令自救，别调用任何外部程序。
  echo 'FAIL: 安装后本机无法执行程序（有规则命中了本机架构），立刻撤销 guard 条目…' >&2
  while IFS= read -r conf; do
    undo_entries "$conf"
  done < <(our_confs)
  # 条目注销后 exec 恢复正常，才能请外部程序帮忙收尾
  remove_alternatives
  rm -f "$CONF_DST" "$CONF_DST.disabled"
  rm -f "$ALT_DIR"/aosc-exec-guard-*.conf
  echo 'guard 条目已撤销、conf 已删掉（qemu 等其它条目没动）。请把本机架构报给上游。' >&2
  exit 1
fi
