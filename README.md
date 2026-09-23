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

  `zz-` 前缀不是随手起的：systemd-binfmt 按文件名排序应用 conf、后应用者优先（实测），`zz-` 让 guard 排在 `qemu-*` 之后 → **guard 先接住外来架构的程序**，再由它决定要不要交给模拟器（见下）。**但这是“手动安装、没有包管理器介入”下的结论**：装到 AOSC OS 上之后，同一架构有好几个包能提供模拟器时，谁生效由 **`update-alternatives` 的优先级**决定——这些包把 conf 放在 `/usr/lib/binfmt.alternatives/`，由 alternatives 链接 `/usr/lib/binfmt.d/emu-<arch>.conf` 指过去（截至 2026-09：qemu 50、FEX 60、latx 80、box64 90；见 abbs 的 `app-virtualization/qemu/29-static-x86_64/build`、`app-emulation/{fex,latx,box64}/autobuild/alternatives`）。这套目前只用在本机跑不了、需要“外来 i386/x86_64”的场合（qemu 的构建脚本只在宿主不是 amd64/i486 时才生成 alternatives 条目），外来架构还是各包直接放 `qemu-<arch>.conf`，所以现在两者能并存、靠文件名分先后；打包进 AOSC 时 guard 怎么参与（单独一个 conf，还是加进同一组候选并定优先级）是那时要定的事，不能指望 `zz-`。

  例如其中 aarch64 那条：

  ```
  :aosc-exec-guard-aarch64:M::\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\xb7\x00:\xff\xff\xff\xff\xff\xff\xff\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff:/usr/bin/aosc-exec-guard:F
  ```

  **装不到本机架构的规则上去**（会劫持解释器自身 → `ELOOP` → 全系统起不了新程序），但不用手工去删：`just install`（底层是 `scripts/install.sh`）按 qemu 的 `qemu-binfmt-conf.sh` **同一套“家族”表**过滤（amd64 上删 i386+x86_64、aarch64 上删 arm+aarch64、mips64 上删 mips 一族…），装到 `/usr` 后还会拿 `/usr/bin/true` 做 exec 冒烟测试，万一过滤漏了它会用内建命令立刻撤销并报错。所以**一份 conf 就够，不需要为每个目标架构各存一份**；打成包时用 `just install <目录>` 或直接 `scripts/install.sh --prefix <目录>`（交叉打包再加目标架构参数，见 `just --list` / `scripts/install.sh --help`）。
- `aosc-exec-guard` 的工作：
  - 读 ELF 头（只读前 20 字节），区分三种情况：外来架构 / 本机架构（本不该被条目命中，防呆）/ 根本不是 ELF；
  - 输出解释到 stderr；如果是从图形会话启动（有 `DISPLAY`/`WAYLAND_DISPLAY`，且 stdout/stderr 都不是终端，也不是 systemd 服务），再弹框（KDE 会话里优先用自带的 Kirigami 框，其次是 kdialog / zenity，见下文“图形弹框”）；
  - 退出码 126，保持 shell 对"找到但无法执行"的惯例。
- 模拟器询问与转发：如果目标架构有**已启用**的 qemu-user binfmt 条目（解释器文件还在），guard 按 `--qemu` / `AOSC_EXEC_GUARD_QEMU` / 用户配置分三种处理：
  - `ask`（默认）：图形会话弹询问框（复选框"不再询问"），终端里给一个上下键菜单（运行一次 / 不运行 / 总是运行，回车确认、`q` 退出；菜单是 dialoguer 自带的 `ColorfulTheme`：黄 `?` 提问、绿 `❯` 指着当前项、确认后绿 `✔` 报告结果，`NO_COLOR` / 非终端下自动不上色）；两种界面都拿不到时（服务、无终端、无对话框）保持老行为——直接交给模拟器；**chroot / 容器里不问，见下文"让位"**；
  - `always`：直接换成模拟器运行，argv 布局与内核调用模拟器时一致（`<解释器> <程序路径> <原参数…>`）；
  - `never`：只解释，不运行。
  勾了"不再询问"（或终端菜单里选了"总是运行"）会把选择写进 `~/.config/aosc-exec-guard.conf`（`qemu = always|never`）；想固定成"从不运行"就手写 `qemu = never`（或 `--qemu=never` / `AOSC_EXEC_GUARD_QEMU=never`），菜单里不再提供这个选项。系统级默认可以放 `/etc/aosc-exec-guard.conf`（打包方/管理员用，用户配置盖过它）。删掉配置文件即可恢复询问。
- 开关：`--no-dialog` / `--debug` / `--qemu=<ask|always|never>`（分别等同 `AOSC_EXEC_GUARD_NO_DIALOG=1` / `AOSC_EXEC_GUARD_DEBUG=1` / `AOSC_EXEC_GUARD_QEMU=…`）。内核调用时命令行只能是"程序路径 + 原程序参数"，没法给 guard 传选项，所以环境变量是内核路径下唯一可用的开关；命令行选项只服务于手动运行。`--qemu` 的优先级：命令行 > 环境变量 > 用户配置（`~/.config/aosc-exec-guard.conf`）> 系统默认（`/etc/aosc-exec-guard.conf`）。手动再记一个：`--handover[=on|off]`（需要 root，把位置让给内核的 qemu 条目，见下文；脚本里加 `--yes`）。
- 命令行解析用 clap：`aosc-exec-guard [选项] <程序路径> [参数…]`——路径之后的参数一律原样保留，`--debug`、`--help` 之类不会被 guard 抢去解析（它们本来就属于原程序）。

## 图形弹框

弹框只在**宿主机的图形会话**里会出现（`mode=Dialog`）；chroot / 容器里没有 session socket，压根走不到这条路，跟静态解释器的性质不冲突。候选按会话挑：

| 会话 | 顺序 |
| --- | --- |
| KDE / Plasma（`XDG_CURRENT_DESKTOP`、`KDE_FULL_SESSION` 等含 kde/plasma） | **Kirigami（`data/dialog.qml`）** → kdialog → zenity |
| 其它桌面 | zenity → kdialog（不把 KDE 味道的框摆到 GNOME 上） |

Kirigami 那个框不是编出来的：它是 `data/dialog.qml`（装到 `/usr/share/aosc-exec-guard/`） + **Qt6 的 qml 运行时**（`/usr/lib/qt6/bin/qml`，包 `qt-6`）+ `kirigami` 的 QML 模块，guard 只负责 exec 它。这样做的好处：

- **guard 自己还是静态单文件**：弹框本来就是子进程（以前是 zenity/kdialog），动态链接的 Qt 只影响这个子进程，而它只在有图形会话的宿主机上跑；找不到（或 QML 跑不起来，比如 qml 退出码不是约定）就往下退，最不济落到终端菜单/只解释。
- Qt6 的 QML 运行时**不是**标准 PATH 里的 `qml`（那台机器上是 Qt5 的，跑不了 Qt6 的 QML），所以 guard 会探测 `/usr/lib/qt6/bin/qml` / `/usr/bin/qml6`，并用 `--version` 确认真的是 6 才用。
- 文案全在 guard 的 `locales/*.yml` 里，用 `--title/--text/--checkbox/--ok/--cancel` 传给 QML（`qml` 运行时的用法是 `qml [选项] <文件> [-- 参数…]`，参数必须在 `--` 之后），QML 里不存字符串，也就没有第二份 i18n。
- 结果用退出码说：**10 = 运行、12 = 运行并记住、11 = 不运行**（0/1/2 是 qml 运行时自己的码，所以避开）；QML 里还带一个 `--test-answer=run|remember|decline` 给测试用（不弹窗直接返回）。
- 框做成**普通对话框窗口**（`flags: Qt.Dialog`）：KWin 画 Breeze 标题栏，和 kdialog 一个观感，不用透明窗口/合成器，也不像 `Kirigami.Dialog`（Popup）那样被宿主窗口尺寸夹住、铺满整个屏幕。踩过的坑：`ApplicationWindow` 不显式写 `visible: true` 就根本不出窗口，而 offscreen 冒烟（只看退出码）看不出来——所以另有一条 `just dialog-smoke`：真会话里把框弹出来、用 xdotool 按回车验退出码，没装 Qt6/xdotool 或没有 `DISPLAY` 就自动跳过。

开关（测试/自定义用）：`AOSC_EXEC_GUARD_DIALOG=<qml|kdialog|zenity>` 钉死用哪个，`AOSC_EXEC_GUARD_QML=<运行时路径>`、`AOSC_EXEC_GUARD_QML_FILE=<qml 文件>` 换个运行时/换自己的框；`--debug` 会打印 `dialog=qml,kdialog,zenity` 这样的候选链，排查“为什么没弹框”先看它。

打包上别让 guard 硬依赖 Qt：`qt-6` + `kirigami` 走 Recommends/Suggests，或者单独一个子包丢给 KDE 桌面 meta；没装它们的机器会自动用 kdialog/zenity，服务器上不装图形工具也能跑。`just test` 里用 stub 运行时验了“KDE 会话优先 Kirigami / 参数与退出码 / 坏了退回 zenity / 非 KDE 不碰它 / `AOSC_EXEC_GUARD_DIALOG` 钉死工具”，另外还有一条真 QML + 真运行时的 offscreen 冒烟（没装 Qt6 就跳过；它只看退出码，窗框到底出没出得来得 `just dialog-smoke`）。

## 目录

```
src/main.rs                  CLI 解析（clap）+ 主流程
src/elf.rs                   ELF 解析、“能不能在本机跑”的判定与解释文案
src/qemu.rs                  模拟器（binfmt_misc 注册表）的发现与转发
src/handover.rs              让位给内核的 qemu 条目（--handover）
src/prompt.rs                询问：图形弹框（Kirigami/kdialog/zenity）、dialoguer 终端菜单、出错弹框
src/platform.rs              环境判定：终端 / 图形 / systemd 服务 / chroot
src/config.rs                设置：--qemu、AOSC_EXEC_GUARD_QEMU、/etc 与用户配置的优先级与读写
src/i18n.rs                  语言判定（文案在 locales/*.yml，由 rust-i18n 编译期嵌入）
locales/en.yml               英文文案（基准语言：缺翻译回退到它）
locales/zh-CN.yml            中文文案
data/dialog.qml              KDE 会话里用的 Kirigami 弹框（Qt6 的 qml 运行时跑，见下文）
justfile                     开发/测试/安装入口（just / just test / sudo just install …）
rust-toolchain.toml         rustup：stable + 各主架构的 musl 标准库（静态构建用）
scripts/install.sh           安装/卸载脚本（just install 就是调它；打包可直接调，不必依赖 just）
data/binfmt.d/zz-aosc-exec-guard.conf.in  规则模板（全集，22 条，抄自 qemu）；安装时由安装脚本过滤成 /usr/lib/binfmt.d/zz-aosc-exec-guard.conf
```

## 使用

本地检查（不需要 root）：

```console
$ just                    # 列出全部配方
$ just build              # 静态构建（唯一构建，见下文“构建”）
$ just test               # 单测 + 直接调用 + stub 弹框/仿真器 + installer
$ just dialog-smoke       # 真会话里弹一次真框（手动看观感/验窗口能显示）
```

装到系统（需要 root；会重启 systemd-binfmt 并做 exec 冒烟测试，失败自动撤销）：

```console
$ sudo just install
# 卸载
$ sudo just uninstall
# 打包/暂存目录（不碰内核）
$ just install /tmp/pkg                  # 目标就是本机架构
$ just install /tmp/pkg aarch64          # 交叉打包指定目标架构
# 打包脚本也可以直接调安装脚本（不依赖 just）：
$ scripts/install.sh --prefix /tmp/pkg --host-arch aarch64
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
$ rm -f ~/.config/aosc-exec-guard.conf   # 用户选择；系统默认在 /etc/aosc-exec-guard.conf
```

## 语言（中文 / English）

guard 的文案（解释文本、终端菜单、zenity/kdialog 弹框、`--handover` 输出、写入配置文件的注释、连 `--help`）都在 `locales/*.yml` 里，一门语言一份文件，由 [rust-i18n](https://crates.io/crates/rust-i18n) 在**编译期**嵌进二进制——运行时不读任何文件，chroot / 容器 / 空 rootfs 里没有 `/usr/share/locale` 也无所谓。

现在有 `locales/en.yml`（基准语言：别的语言缺翻译就回退到它）和 `locales/zh-CN.yml`。选语言的顺序：`AOSC_EXEC_GUARD_LANG` > `LC_ALL` > `LC_MESSAGES` > `LANG`；`zh*` 用中文，其它 locale 先按英文来；一个都没设（服务、chroot 里很常见）默认中文。想临时换一种：

```console
$ AOSC_EXEC_GUARD_LANG=en aosc-exec-guard ./program   # 这一次用英文
$ LC_ALL=en_US.UTF-8 ./program                        # 或者靠 locale 变量
```

它只影响文案，不影响任何判定（转不转发、什么时候问，跟语言无关）。

`--help` 也走同一套文案：说明行、“用法:”标题、`参数:` / `选项:` 两个小节、`-h, --help` / `-v, --version` 两行都跟着语言走。做法照 oma：clap 的属性里直接写 `t!(…)`（oma 写的是 `fl!()`，都是运行时取值）、内置的 help/version 开关禁掉后用本地化文案自己重加、`help_template` 与 `next_help_heading` 管标题。剩下没跟语言走的只有 clap 自己附在取值后面的 `[possible values: …]`（oma 也一样），以及解析出错时 clap 自己的报错。

改动怎么落：

- **加一条消息**：`locales/en.yml` 和 `locales/zh-CN.yml` 各加一行（键用小写连字符，占位符写成 `%{名字}`），代码里写 `t!("键")` / `t!("键", 名字 = 值)`。
- **加一门语言**：照 `en.yml` 抄一份翻掉，放到 `locales/<locale>.yml`（如 `locales/de.yml`，缺翻译的条目不用管，会自动回退英文），在 `Cargo.toml` 的 `[package.metadata.i18n] available-locales` 里加上它，再到 `src/i18n.rs` 的 `locale_for()` 里加一条前缀映射。
- 键写错、占位符对不上、哪种语言漏了键、代码里用了不存在的键、文案里有没人用的键——`cargo test` 里的 `i18n` 测试都会报出来（把 `locales/` 和 `src/*.rs` 两边的键集与占位符对一遍）。
- 还有现成工具：`cargo i18n`（`rust-i18n-cli`）能扫源码把没翻的键抽成 TODO 文件；VS Code 里装 I18n Ally 可以直接看/填翻译。

## chroot / 容器里

binfmt_misc 条目是**宿主**注册的（严格说，是**用户命名空间**级别的内核状态），chroot 隔离不掉它：rootfs 里跑外架构程序，条目照样命中。实测过的事实：

- 我们的 conf 和 qemu 的 conf 都带 **`F`（fix binary）**：解释器文件在**注册时**就被打开、之后一直用那个文件，所以解释器**不要求在 rootfs 里存在**（挂载命名空间、`chroot` 都改不了这一点）。但**静态**解释器才能完全零依赖跑起来——`qemu-user-static` 的 static + `F` 正是这个组合。
- 如果某个条目的解释器是**动态链接**的（比如动态安装的 qemu-user），exec 时它还会去 **rootfs** 里找自己的 `ld.so`/库，找不到就以 `ENOENT` 收场：shell 报“没有那个文件或目录”（`No such file or directory`），而不是 `Exec format error`——别被这句绕进去。guard 自己是全静态的（见“构建”），不吃这个亏。

### guard 在 chroot 里“让位”（默认行为）

guard 认得自己在不在 chroot（比较 `/` 与 `/proc/1/root`）；**看不到 binfmt_misc 注册表时也一并按“让位”处理**（没挂 /proc 的 chroot、容器里就是这样）：那时既没法可靠判断自己在哪，也没法知道内核会怎么处理这个文件。这些环境里它的行为刻意和宿主机不同——**宿主机上的选择、以及“要不要问”本身，都不该带进来**（这是**逐次运行**时发生的让位；要一劳永逸地把 guard 从执行路径里拿掉，见下面「让位给内核的 qemu 条目」）：

- **不问、不带宿主机配置**：忽略 `~/.config/aosc-exec-guard.conf` 与 `/etc/aosc-exec-guard.conf` 里“不再询问”记住的选择（即使那些文件看得见），也不弹询问框（终端里也不会出菜单）；默认直接交给模拟器，让环境里的行为等于**没装 guard 时的行为**。
- **找不到就解释**（并提示：guard 只认注册表，宿主机条目要挂上 /proc 才看得见）。
- 命令行 `--qemu=never|always|ask` 和 `AOSC_EXEC_GUARD_QEMU` 仍然算数：是你显式下的指令，不会被忽略（想在没挂 /proc 的 chroot 里手动要个菜单，就显式写 `--qemu=ask`）。

找不到 binfmt_misc 注册表时（chroot 里很常见），guard **只往宿主机的注册表再问一次**，不猜路径：

1. 进程自己看得到的注册表（rootfs 里挂了 binfmt_misc 时；**挂好了就以它为准**，被禁用的条目不会被绕过）；
2. **宿主机的注册表**：`/proc/1/root/proc/sys/fs/binfmt_misc`——`F` 语义下内核用的就是宿主机那份解释器，照着它转发最忠实（需要 /proc 和权限；**只在共享 PID namespace 时才是“宿主机的”**，见下文“容器”一段）。

两边注册表都说没有，就直接解释退出（不会去看 `/usr/bin/qemu-*` 存不存在）。

于是 chroot 里的体验：rootfs 自己挂了注册表、或者 /proc 在（能借到宿主机条目）→ 外架构程序照常跑；否则 guard 解释原因。按“有没有可达的模拟器条目”分一下：

| 环境 | guard 能转发吗 | 还想让外架构程序跑起来 |
| --- | --- | --- |
| 宿主机 | 能（用本地注册表） | —— |
| chroot（挂了 /proc） | 能（借宿主注册表 `/proc/1/root`） | —— |
| chroot（没挂 /proc） | 不能 | `--handover`（见下）、把 /proc 挂上、或到外面运行 |
| 容器（有自己的 PID namespace） | 不能（`/proc/1/root` 是容器自己的 init，借不到） | `--handover`（在宿主机上做），或让容器自足 |
| 容器（自带注册表 + 自带模拟器） | 能 | —— |

**容器（比如 `systemd-nspawn`）为什么特殊**：`/proc/1/root` 指向容器自己的 init，借不到宿主机的条目；而且装 guard 反而会**挡掉内核本来能做的事**——实测同一个 busybox，装 guard 前靠内核的宿主机 `F` 条目能跑出 `aarch64`，装 guard 后变成解释 + 126；`systemd-nspawn --bind=/proc/sys/fs/binfmt_misc` 也不顶用（nspawn 会挂一份新的 /proc 盖住）。容器想“自足”的话：自己挂上 binfmt_misc（systemd 容器默认会挂）**并且**里面有对应架构的静态 `qemu-*-static`——实测这时 `registry=visible`、guard 能发现条目并照常询问/转发。

### 让位给内核的 qemu 条目（`--handover`）

上面那张表里“不能”的格子，根子是同一个：**guard 转发必须看得见模拟器，而内核不需要**——qemu 条目带 `F`，用的是注册时就打开的宿主机解释器，rootfs / 容器里什么都不用放。guard 挡在最前面，就把它变成了“里面得有 qemu 才行”，正是 `F` 想避免的。

于是一旦在宿主机的终端里跑一次：

```console
$ sudo aosc-exec-guard --handover        # 问一句确认；脚本里可以加 --yes
```

guard 就**把自己从路径里拿掉**：注销全部 `aosc-exec-guard-*` 条目，并把 `/usr/lib/binfmt.d/zz-aosc-exec-guard.conf` 改名成 `.disabled`（systemd-binfmt 只认 `.conf`，重启也不会再注册）。之后外架构程序由内核的 qemu 条目直接处理，宿主机、chroot、容器 行为统一，上面那些“不能”的格子全部消失。恢复：`sudo aosc-exec-guard --handover=off`（会重启 systemd-binfmt 重新注册），或者重新跑安装脚本。

让位这一侧**不需要、也刻意不重启 systemd-binfmt**：内核条目是当场注销的（exec 时才查表，立刻生效），conf 改名后 systemd-binfmt 再怎么重启也只会注册 `qemu-*`，不会把 guard 拉回来。反过来，restart 的 stop 阶段会注销**所有**条目，start 若被 systemd 的启动频率限制挡住就一个都不剩（`--handover=off` 和安装脚本里都先 `reset-failed` 就是为了这个）——让位的全部意义是“让 qemu 接管”，不能拿这个去赌。需要 restart 的只有恢复（`--handover=off`，跑完还会验证条目真的回来了）、重装（安装脚本会删掉 `.disabled` 再重启）和手工改了别的 conf 想立刻生效这几种情况。

代价要说清楚：**让位之后 guard 的询问和解释都不会再出现**；之前选过“总是不运行”的用户，那个选择也随之失效（guard 都不在了）。注册表里没有可用的 `qemu-*` 条目时，它会先警告——让位后外架构程序会直接以 `Exec format error` 失败，没人解释也没人模拟。

只能在**宿主机（外面）**上做：chroot 里（注册表属于宿主机、配置文件属于 chroot）、容器里（PID namespace）都会拒绝并提示到外面跑。测试可以用 `AOSC_EXEC_GUARD_CONF_DIRS` 把它指到别的目录（同时会在测试模式里跳过 systemctl）。

排查这类问题可以用 `--debug`：它会打印 `mode` / `qemu` 条目 / `registry=visible|invisible` / `container=true|false` / tty 等判定条件。

一个诚实的残留：**既没挂 /proc、自己的注册表里也没有可用条目**的 chroot（比如 `chroot /mnt/xx /bin/sh` 这种临时用法）——这种 chroot 里内核本来会用宿主机那份 `F` 解释器把程序跑起来，但 guard 接住后，两边注册表都看不见/没有条目，**没有任何可达的路径可以转发**，只能解释（退 126）。guard 不会去猜 `/usr/bin/qemu-*`，所以往 rootfs 里放模拟器二进制并不能改变这一点；要么把 /proc 挂上，要么 `--handover`，要么到 chroot 外面运行。

另外注意“**guard 能跑起来 ≠ 能把程序跑起来**”：静态 guard + `F` 可以让 guard 进程在空 rootfs 里启动并给出解释，但目标程序还是要靠模拟器——空 rootfs 里没有任何可达的模拟器，guard 一样转不了（这正是 `--handover` 存在的理由）。

构建：**只有静态版，而且就是 musl 静态**——仓库根的 `rust-toolchain.toml` 声明了 stable + 各主架构的 musl 标准库（rustup 装工具链时一并备好），`just build` 按当前宿主机选对应的 musl triple，输出 `target/static/aosc-exec-guard`。装进 `F` 条目的就是它，chroot / 容器 里零拷贝直接可用。动态构建已经删掉：它进了 chroot / 容器 还得连 `ld.so` 和库一起拷（等于把宿主机库带进目标系统），恰恰在最需要它跑起来的地方跑不了。

```console
$ just build             # 输出 target/static/aosc-exec-guard
$ sudo just install      # 装静态版；或手动 install -Dm755 target/static/aosc-exec-guard /usr/bin/aosc-exec-guard
```

（“注销自身”这条路线现在就是上面的 `--handover`：决定在外面做一次，注销条目 + 把 conf 改名，重启后由文件系统状态自然重放。“总是不运行”没法用“注销自身”表达这个问题仍然存在——让位后那个选择就不再生效，已写进上面的代价。）

`just kernel-test` 会真的 chroot 一遍验证整条链路（静态解释器 + `F`：空 rootfs 里零拷贝就能给出解释、挂上 /proc 后认出 chroot 并改提示；能从 `/proc/1/root` 借到宿主机条目时直接让位转发（真程序 busybox 也跑一遍）；没挂 /proc、又借不到条目的 chroot 里提示 `--handover` 这条出路，让位后再进同一个 chroot，内核的 qemu 条目直接把 busybox 跑出 `aarch64`）。

## 内核端到端测试做了什么

1. 先做本机架构防护（本机是 aarch64 就拒绝执行），再注册 `aosc-exec-guard-aarch64` 条目；
2. 注册后立刻用 `/usr/bin/true` 冒烟：本机程序必须还能跑，否则立即中止并清理；
3. 同时保留 `qemu-aarch64` 条目，观察两者谁先匹配（注册顺序语义实测）；
4. 暂时禁用 `qemu-aarch64`，跑一个伪造的 aarch64 ELF 和（若已下载）真实 busybox，检查解释文本与退出码 126；
5. chroot 一遍：宿主条目照样命中；静态解释器 + `F` 让空 rootfs 里零拷贝就能解释，挂上 /proc 后能认出 chroot 并改提示；能从 `/proc/1/root` 借到宿主机条目时直接让位转发（真程序 busybox 也跑一遍）；没挂 /proc 的 chroot 里只能解释、提示 `--handover` 这条出路；让位之后再进同一个 chroot，内核的 qemu 条目直接把 busybox 跑出 `aarch64`；
6. 把 `AOSC_EXEC_GUARD_QEMU=always` 交给真实的 `binfmt_misc` 调用链，让 busybox 经 guard → qemu 跑起来（`uname -m` 输出 aarch64）；
7. 恢复 `qemu-aarch64`、注销 guard，确认原来的模拟器行为回来。

`just systemd-install-test` 另走"真实安装路径"：把 conf 装进 `/usr/lib/binfmt.d/`（解释器指向本仓库构建的二进制）、由 `systemd-binfmt` 应用，再清空条目按文件名顺序重放一遍看优先级，最后移除 conf。

## 已知问题 / 待办

- **与模拟器条目的优先级**（前提是手动安装，见“实测结论”）：binfmt_misc 条目按注册顺序迭代、**后注册者优先**，systemd-binfmt 按文件名排序应用 conf；`zz-aosc-exec-guard.conf` 排在 `qemu-*` 之后，所以**干净启动时 guard 先匹配**，由它询问/转发给模拟器（`AOSC_EXEC_GUARD_QEMU=never` 可让它不插手）。**打包到系统上之后就不是文件名的事了**：同一架构有多个包能提供模拟器时，谁生效由 `update-alternatives` 的优先级决定（`/usr/lib/binfmt.d/emu-<arch>.conf` 指向优先级最高的候选包，见上文“原理”）。不想让 guard 介入的发行版/用户，把 conf 删掉或改名排到 qemu 前面即可，qemu 条目会照旧直接接管；`--handover` 就是把这件事做全（注销条目 + 停用 conf）。
- **`--handover` 的代价**：让位之后 guard 的询问/解释不再出现，“总是不运行”这类用户级选择失效（见「让位给内核的 qemu 条目」）。
- **ENOENT 盲区**：缺解释器的情况（如 32 位程序找不到 `/lib/ld-linux.so.2`、shebang 解释器不存在）报的是 `ENOENT` 而不是 `ENOEXEC`，`binfmt_misc` 拦不到，需要另行设计。
- 文案内置中/英（见上文“语言”），跟判定无关；生产构建就是静态的（`just build`，见上文“构建”）。

## 实测结论（2026-09-22，AOSC OS 13 / x86_64，已装 qemu-aarch64-static；都是手动安装路径，没经过 alternatives）

1. **匹配时机**：`binfmt_misc` 条目在每次 exec 时先行匹配（先于 `binfmt_elf`）；命中即接管，不再尝试原生加载。
2. **优先级**：条目按注册顺序迭代，**后注册者优先**；运行时手工注册（`sudo just kernel-test` 的做法）会盖过开机时就存在的条目。
3. **systemd-binfmt 的顺序**：按文件名排序应用 conf 并逐个（重）注册；冲突时**最后应用的那个胜出**。
4. **于是（手动安装的前提）**：`zz-aosc-exec-guard.conf`（z）排在 `qemu-*.conf`（q）之后 → 干净启动时 guard 后应用、优先级更高 → **guard 先接住外来架构的程序**，再按 `--qemu` / `AOSC_EXEC_GUARD_QEMU` / 用户配置决定是转发给模拟器还是只解释（`just kernel-test`、`just systemd-install-test` 都会验证这一步；这两个配方走的都是“手工把 conf 放进 `/usr/lib/binfmt.d/`”的路径）。
5. **清理**：`systemd-binfmt` 重启会注销"不在配置里"的条目；也可手动 `echo -1 > /proc/sys/fs/binfmt_misc/<条目名>`。
6. **打包注意**：conf 带 `F` 标志 → 注册时解释器文件必须已存在（试过不存在的路径，服务直接报 `No such file or directory` 注册失败）；二进制和 conf 在同一包里安装没问题。另外，**同一架构有多个包能提供模拟器时，谁生效由 alternatives 优先级决定**（`/usr/lib/binfmt.d/emu-<arch>.conf` 链接到优先级最高的候选包，i386/x86_64 家族现为 qemu 50 / FEX 60 / latx 80 / box64 90），guard 真要打包时得按这套机制参与，别依赖 conf 文件名字典序。
7. **端到端行为**：伪造和真实的 aarch64 ELF 都被 guard 接管（中文解释 + 退出码 126）；禁用/恢复 qemu、条目清理均验证通过。
8. **一个 conf 文件可以放多条规则**：`man binfmt.d` 原文是 "Each file contains a list of binfmt_misc kernel binary format rules"，systemd-binfmt 逐行注册、`#`/`;` 开头的注释行忽略（实测：一个文件里两行规则同时注册成功，删掉文件重启后对应条目消失）。
9. **本机架构怎么排除**：不需要为每个目标架构各存一份 conf。qemu 上游（`qemu-binfmt-conf.sh`）是按 CPU **家族**过滤的——i386+x86_64 一族、arm+aarch64 一族、mips 全族、ppc/ppc64 一族、sparc 全族等，`ignore-family=yes`（`just install` 的第三个参数）对应 qemu 的同名选项：同家族但本机跑不了的目标，如 riscv64 上的 riscv32。`just install` 抄的就是这套表，实测：本机 x86_64 装完剩 20 条（去掉 i386+x86_64），目标换成 aarch64 则去掉 arm+aarch64、保留 x86_64。

## 踩坑记录（2026-09-22，实测）

第一版试用了一条匹配全部 ELF 的万能条目（`\x7fELF` + mask `\xff\xff\xff\xff`），结果被内核教训：

- `binfmt_misc` 条目确实在**每次 exec 最先匹配**；命中后解释器是 guard 自身（也是 ELF），于是 guard 再次命中同一条目、再次被替换成 guard……内核解释器递归超限 → `ELOOP`；
- 症状：**全系统无法启动任何新程序**（`bash: /usr/bin/xxx: 符号链接的层数过多`，退出码 126），已运行的进程不受影响；
- 该状态只在内核内存里（没写任何系统配置），**重启即恢复**；但坏着的时候谁也没法自救（连 `sudo`/`ssh` 都启动不了），除非正好有已打开的 root shell 可以用内建命令写 `-1` 清掉条目；
- 结论：条目必须按架构精确匹配、绝不能覆盖本机架构或解释器自身——这也是 `kernel-test.sh` 里那两道防护（本机架构检查 + 注册后冒烟）的由来。
