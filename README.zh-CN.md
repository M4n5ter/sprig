# Sprig

[English](README.md)

**用真实目录并行工作，用内容寻址历史保存值得保留的结果。**

Sprig 为本地实验、构建工具和编码智能体提供独立的原生工作区，以及分支、三方合并和可恢复的编辑操作。你可以像平时一样在普通目录中使用熟悉的工具，再将结果记录为修订。历史通过 BLAKE3 寻址的 FastCDC 内容分块共享数据，无需为每次提交保存一整棵原生目录树。默认的复制／reflink 后端不依赖数据库、守护进程、挂载或网络服务。可选的 Linux 投影后端则在同一套历史与分块存储之上提供挂载式工作区。

Sprig 是本地单用户工具。工作区不是沙箱，检查点也不能为运行中的数据库提供一致性快照。

## 核心模型

- **存储库（store）：** 一个目录，容纳历史、分支引用、工作区和恢复记录。
- **修订（revision）：** 不可变的清单，包含父修订 ID、逻辑时钟和提交说明。清单中的每个条目记录逻辑身份、类型、普通 Unix 权限及修改时间；文件内容表示为有序分块哈希列表。
- **分支（branch）：** 指向某个修订的可移动名称。修订组成有向无环图，普通合并可以有两个父修订。
- **工作区（workspace）：** 关联某个分支和基准修订的真实目录。多个工作区可以共用一个分支，当前文件也可以与已记录的基准不同。
- **工作代（generation）：** 工作区背后的一棵目录树，可以是原生目录或投影视图。托管编辑发布新的一代，同时保留上一代供恢复。

原生克隆与内容去重解决不同的问题。支持克隆的文件系统可以先不复制文件的数据块，就生成可独立写入的工作文件；分块存储则让历史中的相同内容共享数据。两者都不会消除目录扫描、哈希计算、元数据处理或保留工作代的开销。

## 构建并体验

使用带有 C 链接器的 stable Rust 构建。下面的示例适用于 Linux 或 macOS 的 POSIX shell；Windows 的[原生复制子集与构建要求](#windows-原生工作区)见后文。在仓库根目录执行下面的完整示例。它会在临时目录中自行创建一个小项目和新的存储库，不需要准备已有项目。

```sh
cargo build --locked --release
export PATH="$PWD/target/release:$PATH"

DEMO="$(mktemp -d "${TMPDIR:-/tmp}/sprig-demo.XXXXXX")"
DEMO="$(cd "$DEMO" && pwd -P)"
STORE="$DEMO/store"
mkdir "$DEMO/source"
printf 'title=demo\ntheme=light\ncache=on\nworkers=2\nlog=info\n' \
  > "$DEMO/source/settings.conf"
printf 'Initial note\n' > "$DEMO/source/notes.txt"
s() { sprig --store "$STORE" "$@"; }

s init
s import "$DEMO/source" main
s branch baseline main
s fork main experiment

# 托管编辑会自动捕获变更并提交。
s edit main settings.conf 'theme=light' 'theme=dark' -m 'Use a dark theme'
s edit experiment settings.conf 'workers=2' 'workers=4' -m 'Use four workers'

# 普通程序可以编辑原生目录；这些编辑需要显式记录。
s exec experiment -- sh -c 'printf "Try parallel work\n" >> notes.txt'
s checkpoint experiment -m 'Record experiment notes'
s diff-files baseline experiment

# 合并结果写入新分支，两个输入分支都不移动。
s merge main experiment combined
s checkout combined merged-work
s exec merged-work -- cat settings.conf notes.txt
s log combined
s doctor
printf '\nDemo retained at: %s\n' "$DEMO"
```

合并后的配置同时包含 `theme=dark` 和 `workers=4`，笔记中也会多出一行。导入前的源目录不会改变。示例保留在磁盘上，方便继续检查；下文的 `s` 辅助函数仍使用这个存储库。

### 选择复制或还原的对象

- `import SOURCE NAME` 创建工作区、同名分支和初始修订。
- `fork WORKSPACE NAME` 复制**当前工作文件**，包括未提交编辑，并在源工作区的基准修订处创建分支。运行检查点后，这些编辑才进入新分支的历史。
- `checkout BRANCH NAME` 将分支已提交的最新修订重建为一个新工作区，并关联该分支。
- `restore REFERENCE NAME` 将已提交的修订重建为新工作区，同时创建同名新分支。

这些命令不会覆盖已有工作区。历史引用可以是完整修订 ID、分支名或 `workspace:NAME`。显式工作区引用表示其**已提交的基准修订**，不包含未提交文件。`merge OURS THEIRS OUTPUT` 中，OURS 必须是分支，THEIRS 可以是任意历史引用。

## 可选的 Linux 投影后端

使用 `cargo build --locked --release --features projection` 构建，在 `import`、`fork`、`checkout` 或 `restore` 时加上 `--projected`。不加此选项时，创建操作使用现有原生后端；但派生或恢复投影工作区会保留其后端，无需再次指定该选项。macOS 和 Windows 不支持投影；各平台仍默认使用原生工作区。

投影工作代由不可变修订的只读 FUSE 视图与 OverlayFS 可写上层目录组成。文件内容按需从现有 BLAKE3 分块存储读取，验证后进入每个视图最多 16 MiB 的缓存，不另设历史格式。检查点只有在能够证明下层来源以及没有上层变更时才复用下层内容；修改过或无法证明的文件会重新哈希。普通检查点推进分支，但不会更换该工作代固定引用的下层修订，也不会丢弃上层数据。

### 环境要求与示例

投影目前要求**显式以 root 身份执行**、可以访问 `/dev/fuse`、内核支持 FUSE 与 OverlayFS，并有权挂载这两种文件系统。上层所在文件系统也必须支持 OverlayFS；受限容器中的 root 身份本身并不足够。存储库应归 root 所有并保持私有，只运行可信程序：`exec` 不是沙箱，也不会降权。Sprig 不安装特权辅助程序、系统服务或持久权限变更。每个已挂载视图有一个 Sprig FUSE 工作进程，持续到卸载为止。

仅在隔离、可丢弃的 Linux 测试主机上运行以下示例。它新建独立的 root 所有存储库，结束时正常卸载两个视图，但保留数据：

```sh
cargo build --locked --release --features projection
sudo env SPRIG="$PWD/target/release/sprig" sh <<'SH'
set -eu
DEMO="$(mktemp -d /var/tmp/sprig-projection.XXXXXX)"
STORE="$DEMO/store"
mkdir "$DEMO/source"
printf 'hello\n' > "$DEMO/source/note.txt"
s() { "$SPRIG" --store "$STORE" "$@"; }
s init
s --projected import "$DEMO/source" main
s fork main experiment
s exec experiment -- sh -c 'printf "experiment\n" >> note.txt'
s checkpoint experiment -m 'Record projected edits'
s diff-files main experiment
s show experiment
s unmount experiment
s mount experiment
s exec experiment -- cat note.txt
s unmount experiment
s unmount main
s doctor
printf '\nDemo retained at: %s\n' "$DEMO"
SH
```

### 挂载、占用与恢复

- `show`、`list` 和 `status` 报告后端及挂载状态。`mount NAME` 重新打开保留的工作代；需要访问文件的操作也会挂载尚未挂载的投影。`show` 本身不会挂载；直接打开未挂载的路径前，应先运行 `mount`。部分挂载或无效挂载会报错，不会被当作空目录。
- 创建检查点前停止外部写入；卸载或替换工作代前关闭文件句柄，并让进程离开相关工作目录。`exec` 租约只协调 Sprig 操作，无法阻止其他应用读写。
- `unmount NAME` 只执行正常卸载，绝不强制或延迟卸载。挂载被占用时会失败，不会强制清理。若仅卸载了一部分，应检查 `show` 和 `doctor`，关闭仍在使用视图的程序，再正常卸载后重新挂载。保留的上层数据不会被删除。
- 托管编辑与落后工作区的合并会创建替代工作代并保留旧代，发布前必须正常卸载旧视图，因此占用可能阻止发布。之后应重新查询路径。
- `drop workspace NAME` 先正常卸载，再移入可恢复回收区。`recover TOKEN NAME` 保留原条目、恢复原挂载状态，并创建新的投影工作区。对于回收条目中保留的工作代或 `doctor` 报告的孤立工作代，可用 `unmount-generation GENERATION` 正常卸载而不删除数据。启动失败可能留下工作代与诊断日志；重试前先检查，不要直接删除挂载下的目录。

投影拒绝 1970 年之前的修改时间和超过 4,095 字节的符号链接目标，并遵守下文关于可信、停止外部写入的目录树与元数据的限制。挂载、崩溃恢复和性能取决于具体主机；仅通过单元测试不能证明其在特定内核或文件系统上的表现。

## 常见工作流

### 使用熟悉的工具编辑

```sh
s show experiment
s exec experiment -- sh -c 'printf "One more idea\n" >> notes.txt'
s checkpoint experiment -m 'Keep the next idea'
s read experiment notes.txt
s blame experiment notes.txt
```

`show` 的 `path` 字段给出工作区目录。你可以用编辑器打开它、在其中构建，或通过 `exec` 运行程序。外部编辑只有在执行检查点后才进入历史；`commit` 是 `checkpoint` 的别名。清单没有变化时，检查点会复用当前修订，即使提供了不同的提交说明也不会另建修订。

**托管编辑后，或检查点合并了落后于分支的工作区后，应重新查询路径。** 当前工作代可能已改变。切换工作代时暂停外部读写，之后重新打开当前路径。对于原生工作区，旧路径和已打开的文件句柄可能仍指向保留的上一代；替换投影视图则必须先正常卸载旧视图。

`exec` 在工作区目录中运行程序，继承标准输入、输出和错误，并返回子进程的退出状态。运行期间，租约阻止 Sprig 对该工作区执行托管变更。它不会自动创建检查点、停止其他应用，也不会限制文件系统或网络访问。

### 恢复内容，不抹去历史

接着快速体验中的步骤执行：

```sh
s branch saved-result combined
s rollback combined baseline -m 'Return to the initial configuration'
s checkout combined rolled-back
s restore saved-result restored-result
s diff combined saved-result
s status
```

`rollback` 追加一个新修订，使用选定修订的内容，并将当前分支头作为父修订。中间历史仍然保留。它移动分支，**不会改写已有工作区文件**：`merged-work` 仍保留之前的目录树，而 `rolled-back` 包含回滚后的分支头内容。`status` 会列出落后于分支的工作区。

### 显式处理并发编辑

工作区落后于当前分支时，检查点会将本地变更与分支头进行三方合并。独立变更可以合并；冲突变更不会悄悄覆盖较新的历史。`--expected-head REVISION` 还可以要求发布前的分支头必须与指定修订完全一致。

工作区检查点遇到冲突时：

1. 命令失败，分支保持不变。新的冲突工作代成为当前工作代，原目录树仍可恢复。
2. 运行 `show WORKSPACE`，查看当前路径和 `pending_conflicts`。
3. 编辑相关文件并去掉冲突标记。可以使用普通编辑器，也可以使用托管 `write` / `edit` 操作。
4. 对所有列出的冲突执行 `resolve WORKSPACE PATH...`，然后运行 `checkpoint WORKSPACE -m 'Resolve concurrent edits'`。

等待解决冲突时，托管编辑以 `committed: false` 保存新工作代，不推进分支。`resolve` 成功只表示确认路径已处理，并不提交。必须通过冲突标记检查并显式确认；派生工作区和恢复工作区都会保留这些保护。若分支头再次变化，可能还需要再合并一次。

### 实测当前效果

[可复现示例](examples/compare.py) 只使用 Python 3 标准库，自行生成约 12 MiB 的确定性测试数据：两个内容相同的 4 MiB 文件、一个不同的 4 MiB 文件，以及一个小型文本文件。

```sh
cargo build --locked --release
python3 examples/compare.py --binary target/release/sprig

# 在支持的文件系统上，额外要求原生克隆必须成功。
python3 examples/compare.py --binary target/release/sprig --require-clone
```

示例测量普通字节复制基线，以及 Sprig 自动策略和强制复制策略的工作区派生。可选的严格克隆测试必须克隆成功，否则失败；在不支持克隆的文件系统上失败是预期行为。它还检查写入隔离、前缀插入后的分块复用、共用分支的工作区对不重叠编辑的自动合并，以及重叠编辑冲突后的恢复能力。JSON 报告包含运行环境、测试数据大小、操作实测值、断言和命令轨迹。

以下是 **2026-10-22** 在 Linux/x86_64、**tmpfs**、release 构建上的一次实测：

| 操作 | 实际耗时 | 克隆 / 复制的文件数 | 新增 CAS 字节 |
| --- | ---: | ---: | ---: |
| 普通字节复制 | 6.211 ms | 0 / 4 | 不适用 |
| Sprig 派生，自动策略 | 41.251 ms | 0 / 4 | 0 |
| Sprig 派生，强制复制 | 36.626 ms | 0 / 4 | 0 |

测试数据的**逻辑大小为 12,582,984 字节**；由于重复文件复用了内容，初次导入只保存了 **8,388,680 字节的分块**。在一个 4,194,304 字节的文件前插入 **50 字节前缀**后创建检查点，新增了 **157,420 字节的分块**，复用了**原有 52 块中的 51 块**，共 4,036,934 字节。写入隔离、不重叠编辑合并、冲突保留、恢复和完整性断言全部通过。这次运行**没有使用原生克隆**，也未测量独占物理空间。普通复制在这次运行中更快，但它完成的工作也更少。

解读数据时请注意：

- `wall_seconds` 测量整个操作。Sprig 还要扫描、计算哈希、同步数据并发布元数据；普通字节复制基线不提供这些语义。这是可观察的成本，不是同等工作量的加速比基准。
- 逻辑字节数表示文件内容大小，不代表独占磁盘空间。克隆统计说明后端实际做了什么，不代表物理节省量。
- CAS 对象字节增长包含文件分块和元数据页。原生工作区、保留的工作代、清单和备份也占空间；示例没有测量独占物理存储。
- 结果受文件系统、缓存、硬件和负载影响。应在实际使用环境中运行脚本，而不是外推某台机器的一次耗时。

## 合并规则

默认的三方引擎可以组合独立变更、基于逻辑身份的移动，以及不重叠的 UTF-8 文本编辑。内容、删除、位置或结构不兼容时会报告冲突。二进制文件和超过 16 MiB 的文件不做文本合并。内容、权限和修改时间分别合并：单侧修改时间的变化会保留，双侧修改时间均变化时取较大值。托管移动保留身份；对任意原生重命名的识别只是尽力而为。

分支合并总是写入新的输出分支，不改动两个输入。若一个输入的历史已包含另一个输入，则复用后代修订；否则成功时创建双亲修订。未解决的分支合并既不创建输出分支，也不创建修订，更不会创建冲突工作区。两个输入必须拥有共同祖先。

### 按需启用后写优先

左侧输入的 `.lww` 文件选择按时间戳执行后写优先（last-write-wins）的路径。规则支持类似 gitignore 的模式、`!` 否定以及最后匹配优先。`%include` 路径相对于包含它的规则文件，而模式仍相对于目录树根。包含深度上限为 16 层、模式总数上限为 4,096、UTF-8 规则总大小上限为 1 MiB。循环包含、不安全路径，以及缺失或非普通文件的包含目标都会明确失败。落后工作区的合并从当前分支头读取规则。

对受影响的变更，记录的修改时间较大的一侧胜出，相同时选择左侧。删除使用最近的仍存在祖先的时间。`--strategy lww` 将时间戳选择应用于所有冲突；结构冲突按完整关联组件处理，以组件内的最大时间戳决定选择。

重复使用 `--unit a,b` 可以将路径及其子树组成一个合并单元。如果两侧都修改了某单元，则选择成员最新修改时间较大的一侧，**整体采用该侧内容**；相同时选择左侧。单元绕过文本和 `.lww` 决策。单元重叠时，先声明的单元优先。

这些策略依据源文件系统时间，而不是提交顺序，可能舍弃竞争变更。不能接受这种结果时，应使用默认的冲突策略。

## 命令参考

在命令前指定 `--store PATH`。`--json`、`--copy-mode` 和 `--projected` 是全局选项，但 `--projected` 只接受前文列出的四种创建命令。运行 `sprig --help` 或 `sprig --store PATH COMMAND --help` 查看语法。名称由 1–96 个 ASCII 字母、数字、下划线或连字符组成。托管路径相对于工作区；绝对路径和向父目录逃逸的路径会被拒绝。

```text
# 工作区与分支
init
import SOURCE NAME
fork WORKSPACE NEW_NAME
checkout BRANCH NAME
restore REFERENCE NAME
branch NAME REFERENCE
checkpoint WORKSPACE [-m MESSAGE] [--expected-head REVISION]
resolve WORKSPACE PATH...
merge OURS THEIRS OUTPUT [--strategy conflict|lww] [--unit PATH,PATH]...
                        [--expected-head REVISION]
rollback BRANCH REFERENCE [-m MESSAGE] [--expected-head REVISION]
show WORKSPACE
list
status
exec WORKSPACE -- PROGRAM [ARGS...]
mount WORKSPACE
unmount WORKSPACE
unmount-generation GENERATION

# 不可变历史与内容
log [REFERENCE]
diff FROM TO
diff-files FROM TO
blame REFERENCE PATH
read REFERENCE PATH
ls REFERENCE [PATH]
stat REFERENCE PATH
grep REFERENCE PATTERN
glob REFERENCE PATTERN

# 托管编辑：均支持 [-m MESSAGE] [--expected-head REVISION]
write WORKSPACE PATH (--file INPUT | --text TEXT)
edit WORKSPACE PATH OLD NEW
cp WORKSPACE FROM TO
mkdir WORKSPACE PATH
rm WORKSPACE PATH
mv WORKSPACE FROM TO
chmod WORKSPACE OCTAL_MODE PATH

# 恢复与诊断
drop workspace|branch NAME
recover TOKEN NAME
doctor
recover-state GENERATION
gc [--dry-run | --apply PLAN_ID] [--max-extra-bytes BYTES]
retention [--keep-backups N] [--forget-branch-trash TOKEN] [--apply PLAN_ID]
stats
backend
```

### 复制策略

使用原生后端导入、派生，以及从原生工作区目录树恢复时：

- `--copy-mode auto`（默认）：尝试原生克隆，不支持时回退到普通复制。
- `--copy-mode copy`，或 `--copy`：只使用普通复制。
- `--copy-mode clone`：只要有一个普通文件无法原生克隆，操作就失败。

检出、修订还原以及已丢弃分支的恢复会从分块重建文件，因此不使用这些策略。创建或恢复投影工作区同样不使用这些策略。托管编辑先用自动克隆或复制准备变更，再发布到工作区所选的后端。`backend` 描述的是能力，不代表某一对路径一定能成功克隆；仍取决于源与目标所在文件系统的支持情况。Windows 在支持的同卷 ReFS 上使用块克隆，在 NTFS 上普通复制；严格 `clone` 拒绝无法克隆的文件。

### 读取与检查历史

读取命令只解析一次引用，随后检查对应的不可变修订。`read` 流式输出原始字节，不追加换行；配合 `--json` 时则缓存整个文件并返回字节数组。`ls` 列出直接子条目，默认从根目录开始。`stat` 返回已记录的元数据。`grep` 搜索字面文本，跳过二进制、含 NUL 字节或大于 16 MiB 的文件。`glob` 使用 globset 模式；请为 `'**/*.rs'` 等模式加引号，避免 shell 提前展开。

`diff` 报告条目变更和移动。`diff-files` 为 16 MiB 读取上限内的文本提供统一差异补丁；二进制或过大文件的变更会明确标记，不生成补丁。`log` 未指定引用时，从当前所有分支头遍历历史。

`blame` 沿第一父修订历史逐行追溯来源，并通过逻辑身份跟随重命名。未变化的行保留原来的归属；合并引入或修改的行归属于该合并，而不是第二父修订。每行结果包含 `line`、`text`（保留行结束符）、`revision`、`original_line`、`original_path` 和 `message`。输入及参与比较的历史文件必须是无 NUL 的 UTF-8，大小不超过 16 MiB，行数不超过 100,000。最多遍历 10,000 个修订。不支持的内容或超出上限时会失败，而不是返回不完整的归属结果。

### 托管事务与 Rust API

七个托管编辑命令共用 `Store::apply`。它们在私有原生目录树中准备编辑，将已有未提交编辑一起捕获，并把上一工作代保留在可恢复的回收区中。投影工作区在准备完毕后发布新的投影工作代；这条路径仍会生成实际的工作文件。通常工作区与分支头一起发布，返回 `committed: true`；等待解决冲突时则按前文所述处理。

`write --file` 接受任意字节；`--text` 不追加换行。`edit` 要求 UTF-8 文本中的 OLD 恰好匹配一次。`cp` 和 `mv` 的目标必须是一个尚不存在的精确路径。`chmod` 接受 `644`、`0755` 等普通八进制权限位。库调用者可以将多个变更放入一个事务：

```rust
use sprig::{Mutation, Store};

fn add_notes(store: &Store) -> sprig::Result<()> {
    let result = store.apply(
        "main",
        &[
            Mutation::Mkdir { path: "notes".into() },
            Mutation::Write {
                path: "notes/idea.txt".into(),
                content: b"A small experiment\n".to_vec(),
            },
        ],
        "Add experiment notes",
        None, // 可选：预期的分支头修订。
    )?;
    if !result.committed {
        eprintln!("Edits saved; resolve pending conflicts before checkpointing.");
    }
    Ok(())
}
```

库调用者使用投影时，还必须通过 `Store::with_projection_executable` 配置 Sprig 可执行文件，并满足相同的挂载要求。CLI 会自动配置自身可执行文件。

### JSON 与退出状态

`--json` 将结果或错误作为一个格式化 JSON 对象输出到标准输出：

```json
{"schema_version": 1, "result": {"initialized": true, "store": "/path/to/store"}}
```

```json
{"schema_version": 1, "error": {"code": "operation_failed", "message": "..."}}
```

输出结构标识与存储结构标识相互独立。排序是确定的，但生成的 ID 和变化中的状态不是固定测试值。不加 `--json` 时，结果是没有外层封装的 JSON，错误写入标准错误；原始 `read`、帮助和版本输出除外。`exec` 不能与 `--json` 同用。

- `0`：成功。仍应检查 `committed`，因为对待解决冲突工作区的编辑可以成功保存，但不发布修订。
- `1`：操作失败、工作区检查点冲突，或 `doctor` 报告不健康。
- `2`：命令行参数无效。
- `3`：分支合并存在未解决冲突；返回包含 `committed: false` 和冲突详情的结果，而不是错误封装。

`doctor` 发现不健康状态时仍返回完整报告。`exec` 使用子进程的退出状态。发布失败后，重试前先检查存储库状态。

## 恢复与存储

`drop workspace NAME` 保留工作代，包括未提交文件，同时保留分支。执行 `drop branch NAME` 前，必须先丢弃关联工作区。`list` 提供准确的回收条目令牌。`recover TOKEN NAME` 创建新工作区和分支，不消耗原恢复条目。恢复已丢弃分支时重建其已提交修订；恢复已丢弃工作区时保留其当时的目录树。

`doctor` 检查元数据和引用的内容哈希，报告孤立修订、工作代和暂存条目，不自动修复或删除。`gc` 默认只生成非破坏性计划，保护分支头、工作区基准、不可变 lower/owned 引用、冲突候选、回收条目、全部祖先、元数据备份和未发布工作代的 pin。`gc --apply PLAN_ID` 重新核对准确的状态、保留根、备份和对象目录，将活跃块写入经过验证的压缩包，原子发布索引，再仅在没有旧快照读者时永久回收无引用内容。读者繁忙会推迟删除，而非索引发布，因此挂载期间压实可能暂时增加占用。关闭读者、正常卸载后，应重新生成计划再回收。未知文件与未完成准备数据保留；发布结果不确定时不可盲目重试。

压实期间旧包与新包同时存在。计划给出保守额外空间上界，执行默认限额 1 GiB，可用 `--max-extra-bytes` 调整。这不是阻止其他进程消耗磁盘的预留机制；I/O 失败时保留旧或新发布的有效索引和数据。

`retention --keep-backups N` 单独规划旧元数据备份的退役，至少保留两份；`--forget-branch-trash TOKEN` 选择已丢弃的分支引用。使用同样选项加 `--apply PLAN_ID` 才永久执行该计划。保留策略不会删除工作代、脏日志、冲突或发布结果未知的 pin；这些保守根可能阻止回收，不代表自动截断历史或承诺回收全部保留空间。底层对象 API 不建立 Store 根；调用者须通过 Store 事务发布，不能让未引用批次与显式 GC 竞争。

`recover-state GENERATION` 恢复经过验证的元数据备份，即使当前元数据无法正常打开也可使用；当前状态的原始字节会保留供检查。这可能使引用退回较早位置，并让之后的数据不再被引用。它不是普通的内容回滚：应先做好独立备份，谨慎选择 `backups` 中的代号，恢复后运行 `doctor`。

```text
STORE/
  state.json          # 分支、工作区、回收条目和结构标识
  lock                # 元数据协作锁
  objects/            # 不可变数据包、地址索引及对象写入锁
  revisions/HASH.json.zst # 压缩的不可变元数据页根及父修订
  native/GENERATION/  # 原生/投影树或受控日志、固定引用及租约文件
  file-cache/         # 有界、可重建的整文件 clone 来源
  staging/            # 未发布数据和保留的恢复记录
  backups/            # 压缩的历史元数据代
```

修订清单采用按路径排序的规范 JSON 字节流，以内容定义边界分成元数据页。每页独立进行带校验的 Zstandard 压缩，与文件块存入同一不可变对象存储；历史中的未改页共享内容。小型修订记录保存父修订、逻辑时钟、说明及带类型的页根，其规范 JSON 哈希标识修订。读取时检查对象、页和根的域分离哈希、长度、规范排序、路径唯一性及 CDC 边界，再还原公开清单。元数据备份仍是完整 Store 状态的压缩版本；除非显式执行保留策略，否则保留所有恢复根。

当前清单上限为 64 MiB 规范 JSON、一百万条目和 4,096 页。原生捕获仍扫描文件；页共享是存储表示，不代表监控式变化检测或常数时间历史读取。完整清单读取需解码各页，可能比单一压缩文档更慢。普通读取固定一个点查询索引；历史图检查、doctor 和 GC 共用完整验证的对象索引快照。GC 同时保护元数据页和文件块。工作区观察记录和 Store 备份不是共享树。

当前 `state.json` 保持未压缩。若压缩 CPU 成本比空间更重要，可用 `init --no-compression` 为新库选择原始内容；默认选择性压缩，`stats.payload_compression` 报告策略。策略在初始化时固定，不在活动读者下修改。压缩以 CPU 换空间，不代表所有负载都会更快。新内容块仅在至少节省 32 字节且 6.25% 时使用独立带校验 Zstandard 帧，否则保持 raw；地址始终是解压后内容的哈希。原始及解码块均不超过 256 KiB，解压窗口有界，拒绝额外帧和尾部字节。单个不可压新块仍使用 loose；可压块或多块批次使用不可变包。元数据页已有自身编码，无再次压缩收益时自然保持 raw。先同步数据文件与目录，再原子替换带校验索引，完成屏障后才发布修订和分支头。Windows 保留后文较弱的命名空间保证。读者持有固定索引代际的共享 lease，显式删除需要独占 lease，不存在后台自动删包。

添加数据包时会重写当前索引，其磁盘大小上限为 64 MiB。普通单文件读取固定一个索引代际，在检查边界的记录中二分查找，仅对命中记录验证数据包引用和范围，再验证请求的分块哈希和文件总长度。非命中探测不验证数据包引用或范围。它不检查全局索引校验和或所有记录：未访问区域的损坏可能延后发现，排序损坏可能导致对象缺失错误。回退到 loose 对象时必须独立验证请求哈希，但不能据此证明未使用的 packed 别名完好。写入、doctor、GC、批量读取和投影元数据规划仍验证完整索引；批量操作共用已验证快照，按需展开目录元数据。这是明确的容量和增长成本，不代表常数时间更新；仅一个不可压新块的批次不重写索引。压实会重写部分活跃的包，而不是把包内死区算作可原地释放空间。报告区分活跃逻辑字节、新包字节、退役包字节与实际删除字节。

冷分支 `branch NAME REVISION` 仅增加 CAS 引用。`--writable checkout BRANCH NAME` 可建立跨平台、不物化完整内容的未挂载逻辑工作区；普通工具仍需受支持的挂载后端或原生 checkout。
原生物化仅在 APFS、Linux 或 ReFS clone 实际成功时向 `file-cache` 保留验证过的整文件。后续 checkout 逐块验证缓存文件再克隆到独立 inode，减少写盘但不跳过完整文件校验，也不是常数时间 checkout。只能复制的文件系统不会额外保留整文件。目标创建前的缓存错误回退到 CAS，结果不确定的半成品目标不会被覆盖。缓存经同步临时文件发布，可重建；计费限额 256 MiB，按逻辑长度与引用已分配字节的较大值加每项开销计费，并限制 4,096 项。整库统计包含缓存和目录。

原生捕获在文件之间复用一个 256 KiB I/O 缓冲区，但不复用文件内容：即使大小和 mtime 没变，仍会读取每个文件并验证每个已有分块。投影捕获则采用前文所述的下层来源检查。

`stats.object_bytes` 汇总索引和 loose 布局中唯一对象解码后的字节数（文件内容块和元数据页块），包括未引用对象。`object_storage_bytes` 包含对象目录中的所有文件，包括索引、锁、孤立数据包和临时文件。Unix 上的 `object_allocated_bytes` 累加这些文件报告的已分配块，并不代表共享/CoW 存储上的独占物理占用；Windows 返回 null。`reachable_logical_bytes` 累加受保护修订中的文件长度，相同内容在不同修订中会重复计入。`storage` 逐个顶层区域拆账，`store_logical_bytes` 与 `store_allocated_bytes` 包含原生树、upper、日志、修订、备份、缓存及孤立文件；虚拟挂载的 lower/merged 视图不重复计入，实际后备内容已经统计。硬链接别名在每个区域内只计一次。分配量来自 Unix blocks 或 Windows `FILE_STANDARD_INFO`，不代表共享/CoW 卷上的独占物理 extent，也不包含内核/RAM 缓存或文件系统全局元数据。外部正在修改的原生树统计不是原子快照，可复现测量需要先停止写入。`doctor` 列出孤立/临时条目。

当前存储结构（schema）为 **10**。

## 保证与边界

- 工作区之间绝不共享可写硬链接。同一工作区内，普通文件硬链接共享内容、权限与修改时间，同时保留不同的目录项身份。导入、派生、复制与恢复保留内部链接组，但不链接回源工作区。`sprig ln WORKSPACE SOURCE DESTINATION` 通过与其他 SDK、CLI 写入相同的事务引擎创建别名；拒绝目录与符号链接的硬链接。
- 投影工作区首次挂载前，仅将多路径硬链接组物化到私有 OverlayFS upper，其他文件仍按需读取。这避免 `index=off` 时复制写入拆散 lower 链接组，代价是这些组的初始化 I/O 与私有空间。重挂载不会重新填充已删除条目。受控可写工作区直接在 CAS 视图中共享 inode。
- 存储库级协作锁协调 Sprig 的元数据发布，不会冻结其他程序。导入、派生和检查点应使用可信且停止外部写入的目录树。源变更检查只能检测部分竞态，无法检测所有外部写入或恶意祖先路径替换。
- Unix 支持普通文件、目录和符号链接。复制和捕获保留符号链接本身，不跟随目标；之后打开这些链接仍可能访问工作区之外的位置。拒绝特殊文件和非 UTF-8 条目名。源根目录不能是符号链接，源目录与存储库也不能互相包含。Windows 拒绝所有重解析点，包括符号链接和目录联接。
- Unix 上保留普通权限，包括可执行位，并在文件系统时间精度允许的范围内保留记录的修改时间。去掉特殊权限位。不保留所有者、访问或状态变更时间、ACL、扩展属性以及稀疏布局。
- 发布会同步数据并原子替换存储元数据，但不能保证抵御所有文件系统、内核、硬件或断电故障。发布后的报错可能意味着变更实际上已生效；重试前检查 `status`、`list` 和 `doctor`。
- 失败尝试可能留下未发布对象或工作代，不会自动清除。请保持底层存储私有且不被直接修改：不可变性是工具契约，无法阻止操作系统所有者手动改写存储文件。

## 平台与开发

仓库使用 **stable Rust**，包含 rustfmt 和 Clippy。Cargo 声明的最低 Rust 版本为 **1.89**，这不是固定的开发工具链版本。Linux 使用 `FICLONE`，macOS 使用 `fclonefileat`。两者都是逐文件克隆，不是整树快照或 Btrfs 子卷快照。是否支持克隆取决于文件系统。Windows 通过 `FSCTL_DUPLICATE_EXTENTS_TO_FILE` 尝试同卷 ReFS 块克隆；NTFS 使用普通复制，限制见下文。

[CI](.github/workflows/ci.yml) 在 Linux 和 macOS 上运行 stable Rust。macOS 作业设置 `SPRIG_REQUIRE_REFLINK=1`，要求实际 APFS 克隆成功并验证写入隔离。Linux 测试普通路径与回退路径；通过测试不代表该运行环境支持 reflink。示例也在 CI 中执行，macOS 额外要求严格克隆。

### Windows 原生工作区

Windows 后端支持**本地 NTFS 和 ReFS 磁盘上的可写普通文件与目录**，不支持其他 Windows 文件系统。文件身份保留完整 128 位文件 ID 和 64 位卷序列号，不做截断。ReFS 克隆只共享存储 extent，不会在工作区之间共享可写 inode；对齐范围使用块克隆，簇尾不足部分普通复制。小于一个簇的文件在 `auto` 模式下普通复制；严格 `--copy-mode clone` 在文件无法克隆时失败。这不是整树快照，源文件必须保持静止。构建需要 stable Rust 的 MSVC 工具链，以及包含 Windows SDK 的 Visual Studio C++ 构建工具；Zstandard 依赖需要 C 编译器。运行原生工作区不需要文件系统驱动或管理员权限。

在仓库根目录执行以下 PowerShell 示例：

```powershell
cargo build --locked --release
$Sprig = Join-Path $PWD 'target\release\sprig.exe'
$Demo = Join-Path ([IO.Path]::GetTempPath()) ('sprig-demo-' + [guid]::NewGuid().ToString('N'))
$Source = Join-Path $Demo 'source'
$Store = Join-Path $Demo 'store'
New-Item -ItemType Directory -Path $Source -Force | Out-Null
[IO.File]::WriteAllText((Join-Path $Source 'note.txt'), "hello`n")
function s { & $Sprig --store $Store @args; if ($LASTEXITCODE) { throw 'Sprig failed' } }
s init
s import $Source main
s fork main experiment
s write experiment note.txt --text 'hello from Windows' -m 'Edit copy'
s exec experiment -- cmd /c type note.txt
s diff-files main experiment
s doctor
Write-Output "Demo retained at: $Demo"
```

ReFS 的硬链接能力取决于卷格式（3.5 或更新版本）；`backend` 的 `hardlink_support` 返回实际卷标志。不支持链接的卷不能还原包含别名的树，操作会失败而不是拆散链接。

常规 Windows CI 只枚举现有卷，不创建或格式化磁盘。如果存在健康且带盘符的 ReFS 卷，`SPRIG_REFS_TEST_ROOT` 指向该卷，运行严格克隆、非对齐 EOF、身份、检查点、恢复和写隔离测试。没有 ReFS 卷时明确报告缺少运行覆盖，不算克隆已验证；每次 Windows CI 仍独立运行 NTFS 测试。

显式启用的[运行验证](https://github.com/M4n5ter/sprig/actions/runs/40057192603)已在 Windows Server 2025、一次性 4 GiB ReFS VHDX、4 KiB 簇上通过，覆盖严格块克隆、非对齐尾部、跨卷回退和工作区隔离；镜像已正常卸载。默认不创建合成卷，必须显式设置工作流的 `refs_volume` 输入。

Windows 对不支持的行为明确报错，不做静默转换：

- 不支持 Linux 投影或 POSIX `chmod`。支持普通 `exec`，外部修改仍需显式创建检查点。
- 拒绝重解析点，包括符号链接和目录联接，不跟随它们遍历。拒绝 UNC、设备与驱动器相对路径、保留名称、备用数据流路径语法以及不支持的路径别名。请使用普通本地 NTFS 或 ReFS 路径和 UTF-8 名称。
- 文件或目录若包含命名数据流，包括 `Zone.Identifier`，会在捕获或复制前被拒绝，不会静默丢弃这些数据流。
- 捕获时目录记为 `0755`，可写文件记为 `0644`，只读文件会被拒绝。还原时拒绝其他权限模式或精度细于 100 ns 的时间戳，避免静默丢失已记录元数据，因此跨平台还原历史存在限制。
- 新目录继承 Windows ACL；Unix 私有权限位不等于 ACL 安全保证。请将存储库放在仅自己的账户可访问的位置。
- 普通文件会被刷新，元数据替换首先使用写穿重命名。对于仍被读者持有的不可变数据包索引，访问被拒绝时可回退到原子 POSIX 风格替换，并检查重命名前后的文件刷新结果。这要求 Windows 10 1607 或更新版本上的受支持本地卷；缺少所需替换操作时明确失败，不模拟替换。已有读者继续持有旧索引代际。该回退不等同于写穿重命名或目录 fsync，也不宣称与 Unix 后端具有相同的断电后目录命名空间持久性。

Windows 的 `exec` 通过继承的独占访问文件句柄维持租约。在多线程 SDK 宿主中，若其他线程恰好在短暂的句柄继承窗口内绕过 Sprig 启动子进程，该子进程也可能保留租约，使 `workspace_busy` 持续更久。应等这些子进程退出；Sprig 不会强行解除租约，也不提供沙箱。

[Windows CI 联动契约](https://github.com/M4n5ter/sprig/actions/runs/39894835808) 已在托管 Windows runner 上通过，覆盖原生复制、历史与合并、恢复、路径和命名流拒绝，以及继承执行租约。同一轮的 Linux、macOS 原生检查和 rootful Linux 投影工作流也已通过，包括忙句柄拒绝卸载及正常清理。这些合成数据验证不代表完整平台对等或断电持久性证明。

### 受控可写 CAS 工作区

`--writable` 选择由 Sprig 管理写入日志的工作区，而非完整物化目录或 OverlayFS upper。
它共用现有 Store、修订、合并引擎、分支 CAS、可恢复代际与 SDK 事务。import、fork、
checkout 和 restore 创建后**不自动挂载**。SDK 的 write、edit、apply、mkdir、rm、cp、
mv 无需挂载即可使用。Linux 在启用 `--features projection` 后可显式挂载供普通程序访问；
不安装驱动或持久提权组件。Windows 与 macOS 不提供这个可写文件系统适配器。

```sh
sprig --store "$STORE" --writable import "$SOURCE" owned
sprig --store "$STORE" write owned note.txt --text 'hello'
sprig --store "$STORE" writable-status owned
# 以下挂载操作要求 Linux、root 和 /dev/fuse。
sprig --store "$STORE" mount owned
sprig --store "$STORE" exec owned -- sh -c 'printf world >> note.txt'
sprig --store "$STORE" checkpoint owned -m 'ordinary program writes'
sprig --store "$STORE" unmount owned
```

日志准确记录受控修改与稳定文件身份。小写入以有界区间追加在不可变 CAS 内容之上；
截短后再扩展不会重新显示被丢弃的尾部。重命名和删除保留已有打开句柄的身份。
检查点对**整个发生内容变化的文件**重新做 CDC，并非只哈希修改区间；未改文件复用块。
首次访问基础文件会校验其块并建立读取映射。原生和 Overlay 工作区仍要求外部写入者
静止，不因此获得准确脏数据追踪保证。

写入在 fsync、flush 或检查点前是缓冲的。检查点先正常卸载可写视图并取得日志锁；
打开文件或当前工作目录仍被使用时，操作报忙且不发布。绝不强制或延迟卸载。
内容对象与修订持久化后，先固定新干净代际的基础修订，再由一次 Store 状态替换
共同发布分支和工作区。旧日志保留在可恢复回收区，发布结果不确定时不会清空日志。
若提交成功但重新挂载失败，结果包含已提交修订与 `mount_error`，不要盲目重复提交。

Linux 适配器使用缓存直写 I/O，不启用 FUSE writeback-cache 模式。共享映射与普通
写入使用同一个 inode 和日志。成功的 `msync(MS_SYNC)` 或 `fsync` 才是映射数据的
持久化边界，应用必须检查返回值。`MS_ASYNC`、映射仍存活时关闭文件描述符、未同步
的内存写入都不是持久化承诺；映射仍占用挂载时，checkpoint 明确报忙。

挂载前先持久化会话标记。通过 Sprig 正常卸载后，控制端要求匹配的服务进程完成证书、
无剩余句柄、无已观察到的写入或同步失败，并同步日志后才清除标记。进程崩溃、后台写入
失败或停止结果不确定都会保留标记，拒绝普通捕获与重挂载。可用
`recover-writable SOURCE NEW` 显式恢复已入日志的内容，同时保留原代际。绕过 Sprig
手动卸载也缺少此受控确认流程。证书不能证明应用没有忽略内核在请求送达服务进程前
发生的回写错误，也不能恢复从未进入日志的内存内容。

特殊节点、所有者修改、xattr/ACL 和高级重命名标志仍不支持，操作明确失败。
这不是完整 POSIX 文件系统或沙箱。每个代际限制为 256 MiB 日志、单次 1 MiB I/O、
一百万节点，以及基础内容之上 256 GiB 逻辑增长；保留的已删除节点也计入额度。
额度错误不表示宿主磁盘已满；正常检查点轮换健康代际，但不会删除保留日志。

不完整日志尾部或遗留映射会话会使普通捕获失败。对未挂载工作区执行 `writable-status` 可查看已验证
前缀，`recover-writable SOURCE NEW` 显式将此前缀恢复到新工作区并保留原字节。
完整但损坏的记录明确拒绝。完整检查日志前，应先卸载受控工作区再执行 doctor。
实际挂载契约由 `tests/writable_workflow.py` 验证，本地无需挂载的测试不能证明内核支持。

### 开发检查

对这个独立 Cargo 项目，适用之处遵循 [Google Rust Style Guide](https://google.github.io/styleguide/rustguide.html)。

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked

# Linux 投影代码与无需挂载的测试；通过不代表支持实际挂载。
cargo test --locked --features projection

# 仅在支持原生克隆后端的文件系统上执行。
SPRIG_REQUIRE_REFLINK=1 cargo test --locked
```

按需运行的[实际挂载工作流](tests/projection_workflow.py)需要 Python 3，以及具备前述挂载权限、隔离且可丢弃的 Linux 测试环境。它创建独立测试存储库，失败时保留现场，绝不强制或延迟卸载：

```sh
cargo build --locked --features projection
sudo python3 tests/projection_workflow.py --binary target/debug/sprig
```

CLI 是 Rust 库的一层轻量适配。测试覆盖历史、并发工作区、合并、逐行归属、事务、恢复、存储完整性、原生后端和命令行解析。项目尚未选择许可证。

Ubuntu 24.04 的 [Linux 映射工作区检查](https://github.com/M4n5ter/sprig/actions/runs/40002776336)
已验证 C 编译与执行、现有 ELF 执行、私有和共享映射、别名与进程间一致性、`msync`、
映射占用时拒绝检查点以及截断后的 `SIGBUS`。额度耗尽导致后台回写失败后，卸载后的
普通发布仍被阻止，日志保留且可显式恢复。这些是能力检查，不代表完整 POSIX 兼容性或性能提升。

[Linux 硬链接生命周期检查](https://github.com/M4n5ter/sprig/actions/runs/40002776336)
对无外部依赖的小型 Cargo crate 分别开启、关闭增量编译，实际完成首次构建、
无修改构建、源码修改、checkpoint、restore、再次构建，并核对执行产物输出。
无修改与恢复后的构建均为 `Fresh`；增量缓存硬链接成功，未出现 rustc 的复制回退警告。
同一检查覆盖别名写入、链接数与投影链接组重挂载后的隔离。
这不代表所有 Cargo 项目都兼容或更快。
