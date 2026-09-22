# aosc-exec-guard（PoC）

给 AOSC OS 做"程序跑不起来时给出人话解释"的第一步：**架构不兼容的 ELF** 在执行时不再只有一句 `Exec format error`；从图形会话启动时还会弹框说明原因。

对应 macOS 的体验：双击一个跑不了的程序时，会明确告诉你"它不被本机支持"，而不是静默失败。实现完全走内核现成的 `binfmt_misc` 机制，**不需要桌面环境改动，也不需要内核补丁**。

> 范围说明：这是第 1 步 PoC（guard + binfmt 条目）。缺库 / ABI 不兼容（`ld.so` 阶段）的提示、oma 集成、与模拟器条目的产品决策都属于后续步骤，本仓库暂不涉及。

## 原理（含两条实测结论）

- **`binfmt_misc` 条目在每次 exec 时都会被先行匹配**（先于 `binfmt_elf`）。命中后文件直接交给条目的解释器，不再尝试原生加载。
- 因此，条目必须**按目标架构精确匹配**（和 qemu 的 conf 一个套路：用 magic/mask 钉死 ELF 位宽、字节序、`e_machine`、`ET_EXEC|ET_DYN`）；**绝不能**用"匹配所有 ELF"的万能条目——它会把解释器自身（同样是 ELF）也劫持进去，内核的解释器递归到达上限后返回 `ELOOP`，导致全系统无法再启动新程序（见"踩坑记录"）。条目也绝不能匹配**本机架构**，因为 guard 自己就是本机架构。
- 所以本仓库是"一个架构一个条目"：

  ```
  data/binfmt.d/aosc-exec-guard-aarch64.conf     # aarch64（ARM64）
  data/binfmt.d/aosc-exec-guard-riscv64.conf     # RISC-V 64
  data/binfmt.d/aosc-exec-guard-loongarch64.conf # LoongArch 64
  data/binfmt.d/aosc-exec-guard-arm.conf         # ARM（32 位）
  ```

  例如 aarch64 条目：

  ```
  :aosc-exec-guard-aarch64:M::\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\xb7\x00:\xff\xff\xff\xff\xff\xff\xff\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff:/usr/bin/aosc-exec-guard:F
  ```

  （打包时注意：像 qemu 那样过滤掉本机架构，不要把对应本机的 conf 装上系统。）
- `aosc-exec-guard` 的工作：
  - 读 ELF 头（只读前 20 字节），区分三种情况：外来架构 / 本机架构（本不该被条目命中，防呆）/ 根本不是 ELF；
  - 输出解释到 stderr；如果是从图形会话启动（有 `DISPLAY`/`WAYLAND_DISPLAY`，且 stdout/stderr 都不是终端，也不是 systemd 服务），再调 `zenity`/`kdialog` 弹框；
  - 退出码 126，保持 shell 对"找到但无法执行"的惯例。
- 开关：`AOSC_EXEC_GUARD_NO_DIALOG=1` 永远不弹框；`AOSC_EXEC_GUARD_DEBUG=1` 打印决策信息（测试用）。

## 目录

```
src/main.rs                  guard 本体（无第三方依赖）
data/binfmt.d/*.conf         /usr/lib/binfmt.d/ 用的注册项（一架构一个）
scripts/test.sh              本地测试（无 root）：单测 + 直测 + stub zenity 弹框分支
scripts/get-test-binary.sh   下载真实的 aarch64 静态二进制（Alpine busybox-static）
scripts/kernel-test.sh       内核端到端测试（需要 root，自动清理/恢复）
scripts/systemd-install-test.sh 真实安装路径测试（/usr/lib/binfmt.d/ + systemd-binfmt，需要 root）
```

## 使用

本地测试（不需要 root）：

```console
$ scripts/test.sh
```

可选的真实 aarch64 二进制：

```console
$ scripts/get-test-binary.sh   # 然后重跑 scripts/test.sh
```

内核端到端测试（注册 binfmt 条目；脚本结束时会自动注销并恢复 qemu 条目）：

```console
$ scripts/test.sh && sudo scripts/kernel-test.sh
```

如果脚本被强杀导致条目残留（只会影响 aarch64 文件的执行，重启也会清掉），手动清理：

```console
$ sudo sh -c 'echo -1 > /proc/sys/fs/binfmt_misc/aosc-exec-guard-aarch64'
```

## 内核端到端测试做了什么

1. 先做本机架构防护（本机是 aarch64 就拒绝执行），再注册 `aosc-exec-guard-aarch64` 条目；
2. 注册后立刻用 `/usr/bin/true` 冒烟：本机程序必须还能跑，否则立即中止并清理；
3. 同时保留 `qemu-aarch64` 条目，观察两者谁先匹配（注册顺序语义实测）；
4. 暂时禁用 `qemu-aarch64`，跑一个伪造的 aarch64 ELF 和（若已下载）真实 busybox，检查解释文本与退出码 126；
5. 恢复 `qemu-aarch64`、注销 guard，确认原来的模拟器行为回来。

`scripts/systemd-install-test.sh` 另走"真实安装路径"：把 conf 装进 `/usr/lib/binfmt.d/`（解释器指向本仓库构建的二进制）、由 `systemd-binfmt` 应用，再清空条目按文件名顺序重放一遍看优先级，最后移除 conf。

## 已知问题 / 待办

- **与模拟器条目的优先级**：已实测，见"实测结论"——systemd-binfmt 按文件名排序应用 conf、后应用者优先；`aosc-exec-guard-*` 排在 `qemu-*` 前面，**干净启动时 qemu 条目优先**，装了模拟器的用户不受影响。若想让 guard 先"询问"，把 conf 改名排到后面（如 `zz-aosc-exec-guard-*`），或以后在 guard 内部做转发。
- **ENOENT 盲区**：缺解释器的情况（如 32 位程序找不到 `/lib/ld-linux.so.2`、shebang 解释器不存在）报的是 `ENOENT` 而不是 `ENOEXEC`，`binfmt_misc` 拦不到，需要另行设计。
- 文案暂未接 i18n（先用中文）；生产构建建议静态链接。

## 实测结论（2026-09-22，AOSC OS 13 / x86_64，已装 qemu-aarch64-static）

1. **匹配时机**：`binfmt_misc` 条目在每次 exec 时先行匹配（先于 `binfmt_elf`）；命中即接管，不再尝试原生加载。
2. **优先级**：条目按注册顺序迭代，**后注册者优先**；运行时手工注册（`kernel-test.sh` 的做法）会盖过开机时就存在的条目。
3. **systemd-binfmt 的顺序**：按文件名排序应用 conf 并逐个（重）注册；冲突时**最后应用的那个胜出**。
4. **于是**：`aosc-exec-guard-*.conf`（a）排在 `qemu-*.conf`（q）之前 → 干净启动时 qemu 后应用、优先级更高 → **模拟器用户不会被 guard 抢走**；没有模拟器条目的架构则由 guard 解释。
5. **清理**：`systemd-binfmt` 重启会注销"不在配置里"的条目；也可手动 `echo -1 > /proc/sys/fs/binfmt_misc/<条目名>`。
6. **打包注意**：conf 带 `F` 标志 → 注册时解释器文件必须已存在（试过不存在的路径，服务直接报 `No such file or directory` 注册失败）；二进制和 conf 在同一包里安装没问题。
7. **端到端行为**：伪造和真实的 aarch64 ELF 都被 guard 接管（中文解释 + 退出码 126）；禁用/恢复 qemu、条目清理均验证通过。

## 踩坑记录（2026-09-22，实测）

第一版试用了一条匹配全部 ELF 的万能条目（`\x7fELF` + mask `\xff\xff\xff\xff`），结果被内核教训：

- `binfmt_misc` 条目确实在**每次 exec 最先匹配**；命中后解释器是 guard 自身（也是 ELF），于是 guard 再次命中同一条目、再次被替换成 guard……内核解释器递归超限 → `ELOOP`；
- 症状：**全系统无法启动任何新程序**（`bash: /usr/bin/xxx: 符号链接的层数过多`，退出码 126），已运行的进程不受影响；
- 该状态只在内核内存里（没写任何系统配置），**重启即恢复**；但坏着的时候谁也没法自救（连 `sudo`/`ssh` 都启动不了），除非正好有已打开的 root shell 可以用内建命令写 `-1` 清掉条目；
- 结论：条目必须按架构精确匹配、绝不能覆盖本机架构或解释器自身——这也是 `kernel-test.sh` 里那两道防护（本机架构检查 + 注册后冒烟）的由来。
