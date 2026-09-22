# aosc-exec-guard（PoC）

给 AOSC OS 做"程序跑不起来时给出人话解释"的第一步：**架构不兼容的 ELF** 在执行时不再只有一句 `Exec format error`；从图形会话启动时还会弹框说明原因；如果本机装了对应架构的模拟器（qemu-user + 它的 binfmt 条目），还会先问一句"要不要用模拟器运行"（图形弹框里带"不再询问"复选框）。

对应 macOS 的体验：双击一个跑不了的程序时，会明确告诉你"它不被本机支持"，而不是静默失败。实现完全走内核现成的 `binfmt_misc` 机制，**不需要桌面环境改动，也不需要内核补丁**。

> 范围说明：这是第 1 步 PoC（guard + binfmt 条目）。缺库 / ABI 不兼容（`ld.so` 阶段）的提示、oma 集成、与模拟器条目的产品决策都属于后续步骤，本仓库暂不涉及。

## 原理（含两条实测结论）

- **`binfmt_misc` 条目在每次 exec 时都会被先行匹配**（先于 `binfmt_elf`）。命中后文件直接交给条目的解释器，不再尝试原生加载。
- 因此，条目必须**按目标架构精确匹配**（和 qemu 的 conf 一个套路：用 magic/mask 钉死 ELF 位宽、字节序、`e_machine`、`ET_EXEC|ET_DYN`）；**绝不能**用"匹配所有 ELF"的万能条目——它会把解释器自身（同样是 ELF）也劫持进去，内核的解释器递归到达上限后返回 `ELOOP`，导致全系统无法再启动新程序（见"踩坑记录"）。条目也绝不能匹配**本机架构**，因为 guard 自己就是本机架构。
- 所以本仓库是"一个架构一条规则"，但都是**一个** conf 文件里的一行（`man binfmt.d`：一个文件就是"一串规则"，systemd-binfmt 逐行注册，注释行 `#`/`;` 忽略——实测有效）：

  ```
  data/binfmt.d/zz-aosc-exec-guard.conf.in   # 规则模板（全集，22 条）：i386/x86_64/aarch64/arm/armeb/
                                             #   riscv64/loongarch64/mips{,64el,el}/ppc{,64,64le}/s390x/
                                             #   sh4{,eb}/sparc{,32plus,64}/alpha/m68k/microblaze
  ```

  后缀是 `.in` 故意的：它是**模板**（systemd-binfmt 只认 `.conf`，不会误读），安装脚本按目标架构过滤后生成真正的 `zz-aosc-exec-guard.conf`。

  规则里的 magic/mask **逐字节抄自 AOSC OS 的 qemu-user 包**（`/usr/lib/binfmt.d/qemu-*.conf`，20 条），i386/x86_64 两条按同一模板补上（qemu 上游也有，本机因为是 x86_64 被它自己的安装器过滤掉了）——集合要全，别的主机才能拿它解释 amd64 程序。以后要覆盖新架构，从 qemu 的 conf/上游脚本里再抄一行即可。

  `zz-` 前缀不是随手起的：systemd-binfmt 按文件名排序应用 conf、后应用者优先（实测），`zz-` 让 guard 排在 `qemu-*` 之后 → **guard 先接住外来架构的程序**，再由它决定要不要交给模拟器（见下）。

  例如其中 aarch64 那条：

  ```
  :aosc-exec-guard-aarch64:M::\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\xb7\x00:\xff\xff\xff\xff\xff\xff\xff\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff:/usr/bin/aosc-exec-guard:F
  ```

  **装不到本机架构的规则上去**（会劫持解释器自身 → `ELOOP` → 全系统起不了新程序），但不用手工去删：`just install` 按 qemu 的 `qemu-binfmt-conf.sh` **同一套“家族”表**过滤（amd64 上删 i386+x86_64、aarch64 上删 arm+aarch64、mips64 上删 mips 一族…），装到 `/usr` 后还会拿 `/usr/bin/true` 做 exec 冒烟测试，万一过滤漏了它会用内建命令立刻撤销并报错。所以**一份 conf 就够，不需要为每个目标架构各存一份**；打成包时用 `just install <目录>`（交叉打包再加一个目标架构参数，见 `just --list`）。
- `aosc-exec-guard` 的工作：
  - 读 ELF 头（只读前 20 字节），区分三种情况：外来架构 / 本机架构（本不该被条目命中，防呆）/ 根本不是 ELF；
  - 输出解释到 stderr；如果是从图形会话启动（有 `DISPLAY`/`WAYLAND_DISPLAY`，且 stdout/stderr 都不是终端，也不是 systemd 服务），再调 `zenity`/`kdialog` 弹框；
  - 退出码 126，保持 shell 对"找到但无法执行"的惯例。
- 模拟器询问与转发：如果目标架构有**已启用**的 qemu-user binfmt 条目（解释器文件还在），guard 按 `--qemu` / `AOSC_EXEC_GUARD_QEMU` / 用户配置分三种处理：
  - `ask`（默认）：图形会话弹询问框（复选框"不再询问"），终端里问 `[y/N]`（`a`=总是运行、`s`=总是不运行）；两种界面都拿不到时（服务、无终端、无对话框）保持老行为——直接交给模拟器；
  - `always`：直接换成模拟器运行，argv 布局与内核调用模拟器时一致（`<解释器> <程序路径> <原参数…>`）；
  - `never`：只解释，不运行。
  勾了"不再询问"（或终端里回答 `a`/`s`）会把选择写进 `~/.config/aosc-exec-guard.conf`（`qemu = always|never`），删掉该文件即可恢复询问。
- 开关：`--no-dialog` / `--debug` / `--qemu=<ask|always|never>`（分别等同 `AOSC_EXEC_GUARD_NO_DIALOG=1` / `AOSC_EXEC_GUARD_DEBUG=1` / `AOSC_EXEC_GUARD_QEMU=…`）。内核调用时命令行只能是"程序路径 + 原程序参数"，没法给 guard 传选项，所以环境变量是内核路径下唯一可用的开关；命令行选项只服务于手动运行。`--qemu` 的优先级：命令行 > 环境变量 > 用户配置。
- 命令行解析用 clap：`aosc-exec-guard [选项] <程序路径> [参数…]`——路径之后的参数一律原样保留，`--debug`、`--help` 之类不会被 guard 抢去解析（它们本来就属于原程序）。

## 目录

```
src/main.rs                  guard 本体（Rust；只用 clap 做命令行解析）
justfile                     开发/测试/安装入口（just / just test / sudo just install …）
data/binfmt.d/zz-aosc-exec-guard.conf.in  规则模板（全集，22 条，抄自 qemu）；安装时由 just install 过滤成 /usr/lib/binfmt.d/zz-aosc-exec-guard.conf
```

## 使用

本地检查（不需要 root）：

```console
$ just                    # 列出全部配方
$ just test               # 单测 + 直接调用 + stub 弹框/仿真器 + installer
```

装到系统（需要 root；会重启 systemd-binfmt 并做 exec 冒烟测试，失败自动撤销）：

```console
$ sudo just install
# 卸载
$ sudo just uninstall
# 打包/暂存目录（不碰内核）
$ just install /tmp/pkg                  # 目标就是本机架构
$ just install /tmp/pkg aarch64          # 交叉打包指定目标架构
```

可选的真实 aarch64 二进制：

```console
$ just get-test-binary   # 然后重跑 just test
```

内核端到端测试（注册 binfmt 条目；结束时会自动注销并恢复 qemu 条目）：

```console
$ just test && sudo just kernel-test
```

真实安装路径测试（把 conf 装进 `/usr/lib/binfmt.d/`、由 systemd-binfmt 应用，同样自动清理）：

```console
$ sudo just systemd-install-test
```

如果脚本被强杀导致条目残留（只会影响 aarch64 文件的执行，重启也会清掉），手动清理：

```console
$ sudo sh -c 'echo -1 > /proc/sys/fs/binfmt_misc/aosc-exec-guard-aarch64'
```

恢复"每次都问"（清掉"不再询问"记住的选择）：

```console
$ rm -f ~/.config/aosc-exec-guard.conf
```

## 内核端到端测试做了什么

1. 先做本机架构防护（本机是 aarch64 就拒绝执行），再注册 `aosc-exec-guard-aarch64` 条目；
2. 注册后立刻用 `/usr/bin/true` 冒烟：本机程序必须还能跑，否则立即中止并清理；
3. 同时保留 `qemu-aarch64` 条目，观察两者谁先匹配（注册顺序语义实测）；
4. 暂时禁用 `qemu-aarch64`，跑一个伪造的 aarch64 ELF 和（若已下载）真实 busybox，检查解释文本与退出码 126；
5. 把 `AOSC_EXEC_GUARD_QEMU=always` 交给真实的 `binfmt_misc` 调用链，让 busybox 经 guard → qemu 跑起来（`uname -m` 输出 aarch64）；
6. 恢复 `qemu-aarch64`、注销 guard，确认原来的模拟器行为回来。

`just systemd-install-test` 另走"真实安装路径"：把 conf 装进 `/usr/lib/binfmt.d/`（解释器指向本仓库构建的二进制）、由 `systemd-binfmt` 应用，再清空条目按文件名顺序重放一遍看优先级，最后移除 conf。

## 已知问题 / 待办

- **与模拟器条目的优先级**：已实测，见"实测结论"——systemd-binfmt 按文件名排序应用 conf、后应用者优先；`zz-aosc-exec-guard.conf` 排在 `qemu-*` 之后，所以**干净启动时 guard 先匹配**，由它询问/转发给模拟器（`AOSC_EXEC_GUARD_QEMU=never` 可让它不插手）。不想让 guard 介入的发行版/用户，把 conf 删掉或改名排到 qemu 前面即可，qemu 条目会照旧直接接管。
- **ENOENT 盲区**：缺解释器的情况（如 32 位程序找不到 `/lib/ld-linux.so.2`、shebang 解释器不存在）报的是 `ENOENT` 而不是 `ENOEXEC`，`binfmt_misc` 拦不到，需要另行设计。
- 文案暂未接 i18n（先用中文）；生产构建建议静态链接。

## 实测结论（2026-09-22，AOSC OS 13 / x86_64，已装 qemu-aarch64-static）

1. **匹配时机**：`binfmt_misc` 条目在每次 exec 时先行匹配（先于 `binfmt_elf`）；命中即接管，不再尝试原生加载。
2. **优先级**：条目按注册顺序迭代，**后注册者优先**；运行时手工注册（`kernel-test.sh` 的做法）会盖过开机时就存在的条目。
3. **systemd-binfmt 的顺序**：按文件名排序应用 conf 并逐个（重）注册；冲突时**最后应用的那个胜出**。
4. **于是**：`zz-aosc-exec-guard.conf`（z）排在 `qemu-*.conf`（q）之后 → 干净启动时 guard 后应用、优先级更高 → **guard 先接住外来架构的程序**，再按 `--qemu` / `AOSC_EXEC_GUARD_QEMU` / 用户配置决定是转发给模拟器还是只解释（`kernel-test.sh`、`systemd-install-test.sh` 都会验证这一步）。
5. **清理**：`systemd-binfmt` 重启会注销"不在配置里"的条目；也可手动 `echo -1 > /proc/sys/fs/binfmt_misc/<条目名>`。
6. **打包注意**：conf 带 `F` 标志 → 注册时解释器文件必须已存在（试过不存在的路径，服务直接报 `No such file or directory` 注册失败）；二进制和 conf 在同一包里安装没问题。
7. **端到端行为**：伪造和真实的 aarch64 ELF 都被 guard 接管（中文解释 + 退出码 126）；禁用/恢复 qemu、条目清理均验证通过。
8. **一个 conf 文件可以放多条规则**：`man binfmt.d` 原文是 "Each file contains a list of binfmt_misc kernel binary format rules"，systemd-binfmt 逐行注册、`#`/`;` 开头的注释行忽略（实测：一个文件里两行规则同时注册成功，删掉文件重启后对应条目消失）。
9. **本机架构怎么排除**：不需要为每个目标架构各存一份 conf。qemu 上游（`qemu-binfmt-conf.sh`）是按 CPU **家族**过滤的——i386+x86_64 一族、arm+aarch64 一族、mips 全族、ppc/ppc64 一族、sparc 全族等，`ignore-family=yes`（`just install` 的第三个参数）对应 qemu 的同名选项：同家族但本机跑不了的目标，如 riscv64 上的 riscv32。`just install` 抄的就是这套表，实测：本机 x86_64 装完剩 20 条（去掉 i386+x86_64），目标换成 aarch64 则去掉 arm+aarch64、保留 x86_64。

## 踩坑记录（2026-09-22，实测）

第一版试用了一条匹配全部 ELF 的万能条目（`\x7fELF` + mask `\xff\xff\xff\xff`），结果被内核教训：

- `binfmt_misc` 条目确实在**每次 exec 最先匹配**；命中后解释器是 guard 自身（也是 ELF），于是 guard 再次命中同一条目、再次被替换成 guard……内核解释器递归超限 → `ELOOP`；
- 症状：**全系统无法启动任何新程序**（`bash: /usr/bin/xxx: 符号链接的层数过多`，退出码 126），已运行的进程不受影响；
- 该状态只在内核内存里（没写任何系统配置），**重启即恢复**；但坏着的时候谁也没法自救（连 `sudo`/`ssh` 都启动不了），除非正好有已打开的 root shell 可以用内建命令写 `-1` 清掉条目；
- 结论：条目必须按架构精确匹配、绝不能覆盖本机架构或解释器自身——这也是 `kernel-test.sh` 里那两道防护（本机架构检查 + 注册后冒烟）的由来。
