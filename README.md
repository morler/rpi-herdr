# rpi-herdr

把 rpi 接入 [Herdr](https://herdr.dev)：在 Herdr 侧边栏和 `herdr agent list` 里显示
`rpi` 及其 `idle` / `working` / `blocked` 状态，完成或阻塞时收到通知，可用
`herdr agent wait` 做自动化，并让 Herdr 服务器重启后在同一个窗格恢复同一个会话。

这是 rpi 的 Rust `cdylib` 扩展，只依赖 `rpi-plugin-sdk` 与 `serde_json`，**不需要**
修改 rpi 核心。

## 安装

```bash
rpi install rpi-herdr                 # 预编译包
rpi install rpi-herdr --path . --force  # 本地构建
rpi uninstall rpi-herdr               # 卸载
```

支持 Linux、macOS、Windows。

## 生效条件

插件只在同时满足下列条件时才做任何事：

| 变量 | 要求 |
| --- | --- |
| `HERDR_ENV` | 必须严格等于 `1` |
| `HERDR_PANE_ID` | 非空 |
| `HERDR_BIN_PATH` | 非空，且指向一个**存在**的文件 |

`HERDR_SOCKET_PATH` 不使用：v1 全部通过 Herdr CLI 通信，协议文档把它列为可移植路径
（含 Windows）。

在 Herdr 之外（或 `HERDR_ENV` 不是 `1`），插件**注册零个事件处理器并返回成功**：
没有开销，也不会在事件日志里留下记录。

## 状态映射

| rpi 事件 | 得到的状态 |
| --- | --- |
| `BeforeTuiStart` | 置为交互模式并立即 claim 窗格 |
| `SessionStart` | 保持不变（仅记录：本实例确实收到过 start） |
| `BeforeAgentStart`（每轮用户 prompt） | `working` |
| `AgentStart` | `working` |
| `AgentEnd`（完成 / 中止 / 失败） | `idle` |
| `UiPromptStart`（交互框弹出） | `blocked` |
| `UiPromptEnd` | `working` |
| `SessionShutdown` | 释放窗格 |
| 进程退出（`begin_shutdown`） | 释放窗格 |

只有状态**真的变化**时才上报；此外每 10 秒有一次心跳重报，用来把被外部抹掉的归属
自愈回来（见下）。交互框关闭后回到 `working` 而不是 `idle`，因为 rpi 的审批/
`ask_user` 弹框都发生在一次 run 内部，run 尚未结束。

Herdr 可能把"一轮跑完的空闲"显示成 `done` 之类的展示态；上报的线值始终是
`idle` / `working` / `blocked`。

## 只在 TUI 模式上报

headless 运行（`-p` / `--print` / `--mode json` / `--mode rpc` / `--server` /
`--export`）**完全不 claim 也不 release**。原因：Herdr 的 agent 归属是 per
(pane, source) 单槽，而 `release-agent` 会清空整格。旧版本让一次性运行自己 claim
再 release，于是宿主 TUI 正在 `working` 时，模型在 `bash` 里跑一次 `rpi -p` 就会把
父进程从列表里抹掉，直到下一次真实状态变化才回来。

判定依据是**宿主自己的 argv**（`src/env.rs` 的 `is_interactive_process`）。为什么
不能用别的信号（都是实测结论）：

- `BeforeTuiStart` / `SessionStart` 只在进程启动时派发一次。`/reload` 加载的新库
  **不会**再收到它们，用它做门控会让重载后的插件永久静默。
- `--mode rpc` 同样派发 `SessionStart`，无法据此区分。
- stdout 是不是 tty 不行：`rpi -p` 从 shell 启动时继承的是同一个 pty（实测
  `isatty=true`）。
- `GetCliFlag` 本该直接回答这个问题，但宿主在该 action 上 panic
  （`{"error":"runtime_action panicked"}`），不可用。

`rpi dev` / `dev-local` 在同一进程内跑 agent 并把真实参数放在 `--` 之后，所以对它们
只看 `--` 之后的部分；其余情况只看第一个 `--` 之前，避免把 prompt 文本里的 `-p`
当成 flag。

## 配置

| 变量 | 作用 |
| --- | --- |
| `RPI_HERDR=0` | 在 Herdr 内完全禁用（不注册任何处理器） |
| `RPI_HERDR_RESUME=0` | 仍然上报状态，但**不**发送恢复命令 |
| `RPI_HERDR_AGENT=<label>` | 覆盖 `--agent`（默认 `rpi`，不要指向 Herdr 已有集成名） |

`0` / `false` / `off` / `no` / `disabled` 都算关闭（大小写不敏感）。

三个 `HERDR_*` 变量与 `RPI_HERDR_AGENT` 的环境值都会先 `trim()`：Herdr 的窗格 id
与标签按字面匹配，带首尾空格的值不能原样透传。

## 恢复命令（resume）

会话变化时会附带

```
rpi --session-id <id> [--provider <p> --model <m> --thinking <l>]
```

Herdr 用它在服务器重启后于同一个目录重开窗格。构造规则与限制：

- `argv[0]` 必须是 `PATH` 上的普通命令名；不能是路径，也不能带空格（Herdr 会回
  `invalid_resume_argv`）。
- 参数不得含单引号或控制字符；最多 64 个参数、总计 ≤ 8 KiB。
- 以 **argv 数组**通过 `--` 传递。把整条命令拼成一个字符串会被 Herdr 拒绝
  （已实测）。
- 会话 id 来自 `BeforeAgentStart` 的 `sessionId`，回退 `RPI_SESSION_ID`，再回退
  `GetExecutionContext`。`--no-session` 的 `ephemeral` 哨兵不视为可恢复会话；若某个
  候选 id 不安全（含引号/控制字符）会自动降级到下一个候选，而不是丢掉整个命令。
- 模型/供应商/思考级别来自宿主的 `GetExecutionContext`，并在**每一轮开始**重新读取。
  单个值不合法只跳过该项，其余保留；若整组仍超限则退回只含 `--session-id` 的最小命令。
  会话恢复的正确性只依赖会话 id。
- 不使用 `--session` / `-c` / `--resume`：它们语义模糊，可能恢复错会话。

**已知限制**：`--session-dir` 不平移。插件拿不到该 flag 的值（宿主的 `GetCliFlag`
只覆盖扩展自己声明的 flag 与未知长选项）。若会话不在默认目录，请在 Herdr 会话里
设置 `RPI_CODING_AGENT_SESSION_DIR`，或使用 `RPI_HERDR_RESUME=0` 自行管理重开命令。

## 不拖慢 agent

通道协议明确要求上报不能拖慢 agent，因此：

- 事件处理器只把最新请求写进**单槽覆盖式邮箱**然后返回（实测每个 tag 的
  `duration_ms` 都是 `0`）。处理器里不 spawn 进程、不 sleep、不持锁做 I/O。
  静默路径（headless 运行）连邮箱都不碰。
- 只有一个后台线程执行 `herdr`；一次上报还在路上时积累的多个变化只会保留最后一个。
- 每次调用有 2 秒上限，超时即 `kill`；spawn 失败、非零退出、超时一律忽略。
- 三个标准流全部重定向到 null（TUI 独占 tty，且 `herdr` 会往 stdout 写 JSON）。
- `--seq` 用毫秒时间戳并保证单调递增：普通计数器在 `/reload` 后归零会被 Herdr 丢弃。
- `finish_shutdown` 只有在后台线程确实结束后才返回 `0`；否则返回 `-1`，让宿主保留
  本库而不是卸载正在执行的代码。一次失败的 join 是粘性的：后续调用继续返回 `-1`。
- 宿主若没有 lifecycle 钩子，插件拒绝启动（返回非零）：没有 `finish_shutdown` 就没有
  任何东西会 join 上报线程，让宿主卸载它等于在跑代码时开枪打自己。

## 验证

```bash
# 假 herdr，抓取真实 argv
mkdir -p /tmp/fakebin
printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "$FAKE_HERDR_LOG"\n' > /tmp/fakebin/herdr
chmod +x /tmp/fakebin/herdr

# headless：必须一条调用都没有
HERDR_ENV=1 HERDR_PANE_ID=test:pane HERDR_BIN_PATH=/tmp/fakebin/herdr \
  FAKE_HERDR_LOG=/tmp/fake.log rpi -p 'hi'
cat /tmp/fake.log      # 期望：文件不存在或为空

# TUI（需要 pty）：启动即 claim 一次，之后每 10s 一次心跳
python3 - <<'PY'
import os, pty, select, time
env = dict(os.environ, HERDR_ENV="1", HERDR_PANE_ID="test:pane",
           HERDR_BIN_PATH="/tmp/fakebin/herdr", FAKE_HERDR_LOG="/tmp/fake.log",
           TERM="xterm-256color")
pid, fd = pty.fork()
if pid == 0:
    os.execvpe("rpi", ["rpi", "--no-session", "--no-tools"], env)
end = time.time() + 25
while time.time() < end:
    if select.select([fd], [], [], 0.2)[0]:
        try: os.read(fd, 65536)
        except OSError: break
os.kill(pid, 9)
PY
cat /tmp/fake.log

# 真机（建议用独立命名会话，别影响日常会话）
herdr --session rpi-herdr-test
# 在该会话的窗格内运行 rpi，然后：
herdr pane get "$HERDR_PANE_ID"
herdr agent list
herdr agent wait rpi --until blocked --timeout 60000
herdr session stop rpi-herdr-test
herdr --session rpi-herdr-test     # 期望自动执行 rpi --session-id <同一 id>
```

排查：

```bash
RPI_EVENT_LOG=1 rpi            # 每个 handler 的 result/duration_ms
rpi events path && rpi events tail
```

## 已知限制

- **`blocked` 在 v1 只在 TUI 选择器路径上可见**。宿主目前只在
  `open_selector_with_view` / `close_selector` 派发 `UiPromptStart` / `UiPromptEnd`；
  扩展输入框、`ask_user` 的打开/提交路径不派发，所以那种等待仍显示 `working`。
  这是核心侧行为，插件无法补齐。
- **`/reload` 之后宿主仍会给新库派发一次 `SessionShutdown`**，尽管会话并没有结束。
  插件用"本实例从未收到 `SessionStart`"识别这种假释放并跳过它，否则正在 `working`
  的 agent 会被从列表里抹掉。代价：真正的退出路径没有 `SessionStart` 时不会主动
  释放（进程退出后 Herdr 会在 1–2 秒内自己清理窗格）。
- **`release-agent` 会清空整个窗格的 agent 归属**，不按 `--source` 区分（实测）。
  同一窗格里若有别的进程用同一个 pane id 上报，仍会互相覆盖——但 headless 运行现在
  什么都不发，所以最常见的那条路径（模型在 `bash` 里跑 `rpi -p`）已经不会再抢窗格。
- 释放失败不影响正确性：进程退出后窗格回到 shell 提示符，Herdr 会在 1–2 秒内自己
  清理。插件侧的释放只是让这个清理立刻发生。
- 恢复需要 Herdr ≥ 0.9.2；更旧的版本会忽略恢复命令，状态上报与释放照常工作。

## 开发

```bash
cargo build                    # cdylib 给宿主，rlib 给测试
cargo test                     # 纯函数单测，不需要宿主
cargo fmt --check && cargo clippy --all-targets
rpi dev --no-watch             # 编译加载一次
rpi install --path . --force   # 干净安装路径
```

模块划分：

| 文件 | 职责 |
| --- | --- |
| `src/env.rs` | Herdr 环境探测与配置（纯函数，可注入） |
| `src/state.rs` | 3×6 状态转移表（纯函数） |
| `src/resume.rs` | resume argv 构造与 Herdr 约束校验（纯函数） |
| `src/protocol.rs` | `report-agent` / `release-agent` 的 argv 组装 |
| `src/reporter.rs` | 覆盖式邮箱 + 上报线程 + 超时执行器 |
| `src/lib.rs` | `export_plugin!` 入口、事件映射、lifecycle 钩子 |

## 不做的事

- 不直连 Herdr Socket API（CLI 是可移植路径）。
- 不使用 `--agent-session-path`（插件拿不到会话文件路径）。
- 不做自定义状态标签，只用三个标准状态。
- 不给模型提供任何工具：集成不需要模型参与，宿主句柄也不会暴露给模型。
