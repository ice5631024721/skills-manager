# Skills Manager

管理 AI agent 能力资源的桌面应用。资源按类型并列（当前：技能、MCP），各自有中央库，同步到各个 agent 工具。本词汇表定义项目的领域语言。

## Language

### 资源类型

**Resource Type（资源类型）**:
并列管理域的最顶层划分（Skill、MCP，未来可扩展）。决定导航与界面分区；各类型数据模型独立（垂直切片），不共享存储 schema。
_Avoid_: 把某类型的表结构泛化成通用 resource 表

### MCP

**MCP Server（MCP 服务器定义）**:
本系统管理的最小 MCP 单元：一条服务器定义（名字 + stdio 启动命令或远程 URL + env）。指配置条目，不是运行中的进程——stdio 进程由 agent 自己拉起，manager 只做配置管理。当前版本（v1）只识别、不写入。
_Avoid_: MCP 服务（含糊）、把定义与进程混为一谈

**MCP Inventory（MCP 清单）**:
只读扫描受支持 agent 的 MCP 配置得到的本机现状：装了什么 MCP、分别在哪些 agent 里，以及传输类型、启动命令/URL、env 键名、来源文件。按服务器名跨 agent 聚合，是 v1 唯一的 MCP 界面能力。
_Avoid_: 把清单当成中央定义库（清单是现状快照，定义库是将来的写入真源）

**Managed / Foreign Entry（托管条目 / 外来条目）**:
方向已定、v1 未实现：由 manager 导入或新建的条目为托管条目，agent 侧以 manager 为准，可更新、可移除；用户日后直接在 agent 配置里手加的条目为外来条目，只读展示、可一键接管。
_Avoid_: 把外来条目当作托管条目去改写

### 技能

**Skill（技能）**:
一个自包含的技能目录（含 SKILL.md），是本系统管理的最小单元。安装进中央仓库后可同步给多个 agent 工具。
_Avoid_: 插件、命令

### 分组与管理视图

**Skill Source（技能来源）**:
技能的安装出处（一个 git 仓库），在管理视图中自动构成父节点。同一来源下可挂多个技能。
_Avoid_: 仓库组、父节点、源

**Repo Key（仓库规范键）**:
同一仓库所有地址写法（https/ssh、带不带 `.git`、market 的 `owner/name`）归一后的唯一标识。决定技能归属哪个 Skill Source，也是安装去重的判定键。
_Avoid_: 原始 URL、source_ref

**Source Type（来源类型）**:
技能的来源种类：`git`、`skillssh`、`local`、`import`。描述"从哪类渠道来"，不指向具体仓库；与 Skill Source 是两个概念。
_Avoid_: 把来源类型当作 Skill Source

**Source Refresh（整仓更新）**:
一次 clone 批量更新同一个 Skill Source 下全部已装技能的操作。发现上游新增技能时提示确认后再安装；上游已删除的技能本地保留并标记。
_Avoid_: 与逐技能的批量更新混用

**Tag（标签）**:
用户给技能打的自由文本标记。与分组正交叠加使用，不构成层级。
_Avoid_: 用标签充当分组

### 同步

**Agent Workspace（agent 工作区）**:
某个 agent 工具（如 Claude Code、Codex）存放技能的目标目录。技能同步到此目录时保持平铺，不携带管理视图的分组结构。
_Avoid_: 项目（与用户代码项目混淆时）
