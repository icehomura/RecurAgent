# CLI 命令参考

## `ra chat`

交互式多轮对话，支持 readline 历史记录。

```
ra chat [OPTIONS]

Options:
  -c, --cwd <PATH>         工作目录
      --config <PATH>      配置文件路径
      --provider <NAME>    LLM 供应商
      --model <NAME>       模型名称
      --base-url <URL>     自定义 API 端点
  -m, --message <MSG>      单条消息（非交互模式）
      --max-iterations <N> 每条消息的最大工具迭代次数（默认：50）
  -v, --verbose            显示工具输出
      --no-retry           禁用重试
```

**功能特性：**

- 方向键和行编辑（rustyline）
- 持久化历史记录，保存在 `.ra/history/chat_history`
- 退出方式：`/exit`、`/quit`、`exit`、`quit`、`:q`、Ctrl+C、Ctrl+D
- 完整工具访问（Shell、文件、搜索、Web）

**示例：**

```bash
ra chat                              # 交互模式（默认）
ra chat --provider deepseek          # 使用 DeepSeek
ra chat --model glm-4-plus           # 自动识别为智谱
ra chat --message "Fix auth bug"     # 单条消息，执行后退出
```

---

## `ra gateway`

以常驻多渠道守护进程方式运行。

```
ra gateway [OPTIONS]

Options:
  -c, --cwd <PATH>         工作目录
      --config <PATH>      配置文件路径
      --provider <NAME>    覆盖供应商
      --model <NAME>       覆盖模型
      --base-url <URL>     覆盖 API 端点
  -v, --verbose            详细日志
      --no-retry           禁用重试
```

需要在配置文件中包含 `gateway` 部分及 `channels` 数组。持续运行直至按下 Ctrl+C。

---

## `ra init`

初始化工作区，创建配置和引导文件。

```
ra init [OPTIONS]

Options:
  -c, --cwd <PATH>    工作目录
      --defaults       跳过交互提示，使用默认值
```

**创建内容：**

- `.ra/config.json` -- 供应商/模型配置
- `.ra/.gitignore` -- 忽略状态文件
- `.ra/AGENTS.md` -- 智能体指令模板
- `.ra/SOUL.md` -- 个性模板
- `.ra/USER.md` -- 用户信息模板
- `.ra/memory/` -- 记忆存储目录
- `.ra/sessions/` -- 会话历史目录
- `.ra/skills/` -- 自定义技能目录

---

## `ra status`

显示系统状态。

```
ra status [OPTIONS]

Options:
  -c, --cwd <PATH>    工作目录
```

**输出示例：**

```
ra Status
══════════════════════════════════════════════════

Config:    .ra/config.json (found)
Workspace: .ra/            (found)
Provider:  anthropic
Model:     claude-sonnet-4-20250514

API Keys
──────────────────────────────────────────────────
  Anthropic    ANTHROPIC_API_KEY         set
  OpenAI       OPENAI_API_KEY           not set
  ...

Bootstrap Files
──────────────────────────────────────────────────
  AGENTS.md        found
  SOUL.md          found
  USER.md          found
  TOOLS.md         missing
  IDENTITY.md      missing
```

---

## `ra serve`

启动 Web 界面和 REST API 服务器。需要在编译时启用 `api` 特性。

```bash
cargo install --path crates/ra-cli --features api
ra serve                               # 绑定到 127.0.0.1:50080
ra serve --host 0.0.0.0 --port 50080   # 接受外部连接
ra serve --solo                        # 启用本地免密码「solo」登录
ra serve --stdio                       # AppUI JSON-RPC 走 stdin/stdout（不绑定 HTTP）
```

主要选项：

| 参数 | 说明 |
|------|------|
| `--port <N>` | 监听端口（默认 `50080`，位于 IANA 动态端口范围） |
| `--host <ADDR>` | 绑定地址（默认 `127.0.0.1`；外部访问用 `0.0.0.0`） |
| `--stdio` | 通过 stdin/stdout 运行 AppUI JSON-RPC 协议，而非 HTTP |
| `--solo` | 启用仅回环的免密码 solo 登录（`POST /api/auth/solo*`）；也可用 `RA_SOLO_LOGIN=1`。切勿在反向代理之后启用 |
| `--data-dir <P>` | episodes/记忆/会话的数据目录（默认 `$RA_HOME` 或 `~/.ra`） |
| `--auth-token <T>` | API 访问的管理员 Bearer 令牌 |
| `--config <P>` | 配置文件路径 |
| `--swarm-backend <stdio\|http>` | 启用 `/api/swarm/*` 契约创作端点（配合 `--swarm-backend-cmd` / `--swarm-backend-url`） |

在 `/app/`（聊天/studio）和 `/admin/`（运维仪表盘）提供内嵌 SPA，并在 `/api/ui-protocol/ws` 提供 WS UI Protocol。`/metrics` 端点提供 Prometheus 格式的指标（`ra_tool_calls_total`、`ra_tool_call_duration_seconds`、`ra_llm_tokens_total`）。使用不同的 `--data-dir` + `--port` 可并行运行多个实例。

---

## `ra clean`

清理数据库和状态文件。

```bash
ra clean [--all] [--dry-run]
```

| 参数 | 说明 |
|------|------|
| `--all` | 移除所有状态文件 |
| `--dry-run` | 仅显示将被删除的内容，不实际执行 |

---

## `ra completions`

生成 Shell 自动补全脚本。

```bash
ra completions <shell>
```

支持的 Shell：`bash`、`zsh`、`fish`、`powershell`。

---

## `ra cron`

管理定时任务。

```bash
ra cron list [--all]                  # 列出活跃任务（--all 包含已禁用的）
ra cron add [OPTIONS]                 # 添加定时任务
ra cron remove <job-id>               # 移除定时任务
ra cron enable <job-id>               # 启用定时任务
ra cron enable <job-id> --disable     # 禁用定时任务
```

**添加任务：**

```bash
ra cron add --name "report" --message "Generate daily report" --cron "0 0 9 * * * *"
ra cron add --name "check" --message "Check status" --every 3600
ra cron add --name "once" --message "Run migration" --at "2025-03-01T09:00:00Z"
```

Cron 表达式使用标准语法。任务支持可选的 `timezone` 字段，使用 IANA 时区名称（如 `"America/New_York"`、`"Asia/Shanghai"`）。未指定时默认使用 UTC。

---

## `ra channels`

管理消息渠道。

```bash
ra channels status    # 显示渠道的编译/配置状态
ra channels login     # WhatsApp 二维码登录
```

status 命令会显示一张表格，包含渠道名称、编译状态（特性标志）和配置摘要（环境变量的设置/缺失情况）。

---

## `ra office`

Office 文件操作（DOCX/PPTX/XLSX）。核心操作使用原生 Rust 实现，无需外部依赖；少数子命令在安装了 LibreOffice（`soffice`）时会可选地调用它。

```bash
# 核心（纯 Rust）
ra office extract <file>                     # 提取文本为 Markdown
ra office unpack <file> <output-dir>         # 解包为格式化的 XML
ra office pack <input-dir> <output>          # 将目录打包为 Office 文件
ra office clean <dir>                        # 清理解包后 PPTX 中的孤立文件
ra office validate <file>                    # 校验 Office 文件的结构
ra office make-slide <image> -o <pptx>       # 将背景图 + --texts JSON 叠加文本合成为一张 .pptx 幻灯片
ra office add-slide <unpacked-dir> <source>  # 向解包后的 PPTX 添加幻灯片（复制 slideN.xml 或套用 slideLayoutN.xml）
ra office overlay-text <image> <text>        # 将文本烧录到 PNG/JPEG 图片上（--x/--y 定位）
ra office comment <unpacked-dir> <id> <text> # 向解包后的 DOCX 添加批注

# 依赖 LibreOffice（需 PATH 中有 `soffice`）
ra office accept-changes <input> <output>    # 接受修订（DOCX）→ 干净副本
ra office recalc <file>                      # 重新计算 XLSX 公式
ra office thumbnail <file> [OPTIONS]         # 渲染幻灯片/页面缩略图（还需 Poppler 的 pdftoppm）
ra office soffice <args...>                  # 透传到沙箱化的 soffice
```

`make-slide` 将渲染好的背景图与 JSON 叠加文本合成为一张 `.pptx` 幻灯片（供 slides 流水线使用）。`comment` 会将文本原样插入 DOCX XML，因此需传入**已转义**的 XML（`&amp;`、`&lt;` 等）。Office 是**仅 CLI** 功能——不作为 agent 工具暴露。运行 `ra office <子命令> --help` 查看确切参数。

---

## `ra account`

管理 Profile 下的子账户。子账户继承 LLM 供应商配置，但拥有独立的数据目录（记忆、会话、技能）和渠道。

```bash
ra account list --profile <id>                         # 列出子账户
ra account create --profile <id> <name> [OPTIONS]      # 创建子账户
ra account update <id> [OPTIONS]                       # 更新子账户
```

---

## `ra auth`

OAuth 登录和 API 密钥管理。

```bash
ra auth login --provider openai           # PKCE 浏览器 OAuth
ra auth login --provider openai --device-code  # 设备码流程
ra auth login --provider anthropic        # 粘贴令牌（标准输入）
ra auth logout --provider openai          # 移除已存储的凭据
ra auth status                            # 显示已认证的供应商
```

凭据存储在 `~/.ra/auth.json`（文件权限 0600）。解析 API 密钥时，优先检查凭据存储，其次才是环境变量。

---

## `ra skills`

管理技能。

```bash
ra skills list                            # 列出已安装的技能
ra skills install user/repo/skill-name    # 从 GitHub 安装
ra skills remove skill-name               # 移除技能
```

从 GitHub 仓库的 main 分支获取 `SKILL.md` 并安装到 `.ra/skills/`。

---

## `ra doctor`

运行本地环境诊断并打印 RecurAgent 服务器的健康报告。

```bash
ra doctor [OPTIONS]

选项：
      --json          输出机器可读的 JSON 支持包
      --verbose       在每行附加解析出的路径/版本
      --strict        将警告提升为失败（影响退出码）
      --data-dir <P>  数据目录覆盖（默认 ~/.ra）
```

检查已安装二进制的位置（及 PATH 遮蔽）、终端（terminfo）、config/数据目录的可写性、UI 协议版本偏移，以及 `api.github.com` 的可达性（用于更新检查）。（它**不**校验供应商 API 密钥——请用 `ra status` 检查那些。）任一检查失败时退出码非零（加 `--strict` 时任一警告也会致失败）。用 `--json` 将支持包附到缺陷报告。

---

## `ra docs`

为内置工具和供应商生成参考文档。

```bash
ra docs [--output <DIR>]
```

未指定 `--output` 时将 Markdown 写到标准输出；否则创建 `<DIR>` 并写入 `<DIR>/TOOLS.md`。输出涵盖内置工具，以及一份**硬编码**在命令中的供应商列表，因此可能落后于实际的供应商注册表。

---

## `ra memory`

查看并驱动记忆刷新（memory-refresh）流水线（见[记忆与技能](./memory-skills.md)）。

```bash
ra memory refresh [--data-dir <P>]           # 立即执行一次抽取
ra memory status  [--data-dir <P>]           # 扫描状态：锁持有者、待处理积压、预算
ra memory remember "<text>" [--data-dir <P>]  # 由宿主直接记住（无模型参与）
ra memory forget  "<text>" [--sensitive]      # 自由文本遗忘（进入确认流程）
ra memory forget  --id ^m4k2abq               # 硬删除某条精确的 MEMORY.md 条目
```

即使配置中禁用了后台扫描，`refresh` 仍可运行；但当运行中的服务持有 profile 锁时会拒绝执行。`remember`/`forget` 只写入**本地暂存笔记**（写入时不调用 LLM）；该笔记会在下一次整合（后台扫描或 `ra memory refresh`）时应用，而那一步*会*把它发送给整合模型。`--sensitive` 会立即临时归档候选内容，并在确认后在各处彻底清除。

---

## `ra update`

检查是否有更新的 RecurAgent 版本。

```bash
ra update --check         # 打印更新方案；有更新可用时退出 10，已是最新时退出 0
ra update --check --json  # 同上，机器可读
```

这是 Stage-2 的**仅检查**命令：它识别安装来源（Homebrew、cargo、cargo-dist receipt……）并打印对应的升级命令。原地应用更新属于 Stage 3，**尚未接入**——请运行打印出的命令来升级。

---

## `ra mcp-serve`

将 RecurAgent 自身暴露为 MCP 服务器，供外层编排器将其作为子 agent 调用。

```bash
ra mcp-serve [OPTIONS]

选项：
      --transport <stdio|http>  绑定的传输方式（默认：stdio）
      --bind <ADDR>             HTTP 传输的绑定地址（默认：127.0.0.1:4033）
  -c, --cwd <PATH>              工作目录
```

`stdio` 使用父进程信任认证（JSON-RPC 走 stdin/stdout）。`http` 是最小的 HTTP/1.1 JSON-RPC 端点，**必须**通过环境变量 `RA_MCP_SERVER_TOKEN` 提供 Bearer 令牌。

---

## `ra admin`

面向托管/集群部署的租户与隧道管理（frps 反向隧道接入）。大多数单用户安装无需使用。

```bash
ra admin create-tenant --name <id> [OPTIONS]   # 分配子域名、认证令牌、SSH/serve 端口
ra admin list-tenants                          # 列出已注册的隧道租户
ra admin delete-tenant <id>                    # 删除租户
ra admin show-tenant-config <id>               # 打印某租户的 frpc 配置
ra admin reset-token                           # 重置管理员令牌（恢复引导令牌认证）
ra admin set-smtp-password                     # 写入 smtp_secret.json（0600）用于 OTP 邮件
ra admin operator-summary [--base-url <URL>] [--auth-token <TOK>]  # 精简的运行时可观测性视图
```

`create-tenant` 默认基础域名为 `ra-cloud.org`、本地 serve 端口为 `50080`（与 `ra serve` 一致）。`reset-token` 与 `set-smtp-password` 作用于本地 `--data-dir`；`operator-summary` 查询运行中的 API。
