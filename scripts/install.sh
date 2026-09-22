#!/usr/bin/env bash
# aosc-exec-guard 安装器：装二进制 + 生成按本机架构过滤过的 binfmt conf。
#
#   scripts/install.sh                        # 装进 /usr，重启 systemd-binfmt 并做 exec 冒烟测试
#   scripts/install.sh --prefix /tmp/pkg      # 只写文件（打包/staging 用，不碰内核）
#   scripts/install.sh --host-arch aarch64    # 交叉打包时指定目标架构（默认 uname -m）
#   scripts/install.sh --ignore-family yes    # 同家族但本机跑不了的目标（如 riscv64 上的 riscv32）
#   scripts/install.sh --uninstall            # 卸载（--prefix 同理）
#
# 过滤逻辑和 qemu 的 scripts/qemu-binfmt-conf.sh 一致：把与本机**家族**相同的规则整族
# 删掉（amd64 上删 i386+x86_64、aarch64 上删 arm+aarch64、mips64 上删 mips 一族…）。
# 卸不掉本机架构的规则会让条目劫持解释器自身：内核解释器递归到 ELOOP，全系统起不了新程序。
# 装到 /usr 时脚本还会用 /usr/bin/true 做冒烟测试，失败就立刻撤销（只用内建命令，因为那时
# exec 已经坏了）。
set -euo pipefail
cd "$(dirname "$0")/.."

CONF_SRC=$PWD/data/binfmt.d/zz-aosc-exec-guard.conf
BIN_SRC=$PWD/target/release/aosc-exec-guard

PREFIX=/usr
PREFIX_GIVEN=no
TARGET_ARCH=${HOST_ARCH:-}   # 空 = 用 uname -m
IGNORE_FAMILY=no
UNINSTALL=no

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX=$2; PREFIX_GIVEN=yes; shift 2 ;;
    --host-arch) TARGET_ARCH=$2; shift 2 ;;
    --ignore-family) IGNORE_FAMILY=$2; shift 2 ;;
    --uninstall) UNINSTALL=yes; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "未知参数：$1" >&2; usage >&2; exit 2 ;;
  esac
done

BIN_DST=$PREFIX/bin/aosc-exec-guard
CONF_DST=$PREFIX/lib/binfmt.d/zz-aosc-exec-guard.conf
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

restart_binfmt() {
  systemctl restart systemd-binfmt.service 2>/dev/null ||
    echo '（systemctl 重启 systemd-binfmt 没成功：不是 systemd 系统？）' >&2
}

if [ "$UNINSTALL" = yes ]; then
  # 先记下要注销的条目（conf 删掉后就看不到了）
  undo_list=$(mktemp)
  if [ -e "$CONF_DST" ]; then
    cp "$CONF_DST" "$undo_list"
  fi
  rm -f "$BIN_DST" "$CONF_DST"
  if [ "$PREFIX_GIVEN" = no ]; then
    restart_binfmt
    undo_entries "$undo_list"   # 保险：万一服务没把条目注销掉
  fi
  rm -f "$undo_list"
  echo "已卸载 $BIN_DST 与 $CONF_DST"
  exit 0
fi

[ -x "$BIN_SRC" ] || {
  echo "找不到 $BIN_SRC；先 cargo build --release（或 scripts/test.sh）。" >&2
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
install -Dm644 "$tmp_conf" "$CONF_DST"
printf '已写入 %s\n已写入 %s（%s 条规则，已去掉 %s 家族的规则）\n' \
  "$BIN_DST" "$CONF_DST" \
  "$(grep -c '^:' "$CONF_DST")" "$HOST_FAMILY"

if [ "$PREFIX_GIVEN" = yes ]; then
  echo '（--prefix 模式：没有碰内核；装到目标系统时请在那里执行 scripts/install.sh）'
  exit 0
fi

restart_binfmt
if /usr/bin/true 2>/dev/null; then
  echo '  （本机程序执行正常）'
  echo "安装完成：$(grep -c '^:' "$CONF_DST") 条规则已生效。"
else
  # 到这儿说明有条规则命中了本机架构：只能靠内建命令自救，别调用任何外部程序。
  echo 'FAIL: 安装后本机无法执行程序（有规则命中了本机架构），立刻撤销 guard 条目…' >&2
  undo_entries "$CONF_DST"
  rm -f "$CONF_DST"
  echo 'guard 条目已撤销、conf 已删掉（qemu 等其它条目没动）。请把本机架构报给上游。' >&2
  exit 1
fi
