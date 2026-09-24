# Skills Manager

管理 AI agent 技能（Skills）的桌面应用：技能从各处安装进中央仓库，再同步到各个 agent 工具目录。本词汇表定义项目的领域语言。

## Language

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
