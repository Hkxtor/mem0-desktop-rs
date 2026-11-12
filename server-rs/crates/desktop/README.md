# mem0-desktop (Rust 版)

一个自带一切的本地 Mem0 桌面应用：REST API + 网页 dashboard 打包进**单个二进制**，不需要安装 Python、Node 或任何数据库。解压后双击运行。

## 运行

```bash
# Linux / macOS
./mem0-desktop

# Windows
mem0-desktop.exe

# 只跑服务、不开窗口（服务器 / 没有桌面环境 / CI 里用这个）
./mem0-desktop --headless
```

启动后窗口会自动打开 dashboard（也可浏览器访问 `http://127.0.0.1:<端口>`，端口见启动日志；默认优先 8888，被占用时自动选空闲端口）。

第一次启动会引导你填入 LLM / embedding 的 provider 和 API key（`/setup`）。这些配置存在本机 `data/` 目录里，不会上传。

## 数据在哪

默认在**可执行文件旁边的 `data/` 目录**——整个文件夹可以随意搬移，U 盘上也能用。也可以用环境变量指定：

```bash
MEM0_DATA_DIR=/path/to/data ./mem0-desktop
```

```
mem0-desktop/
├── mem0-desktop       # 可执行文件（dashboard 已内嵌）
└── data/              # 所有状态都在这里（首次启动自动创建）
    ├── config.json    # provider 配置（含 API key，注意保密）
    ├── mem0.db        # 用户、API key、请求日志、设置（SQLite）
    ├── history.db     # 记忆变更历史（SQLite）
    ├── vectors.db     # 向量数据（sqlite-vec）
    ├── logs/app.log   # 应用日志（JSONL，5 MB × 3 份自动轮转）
    └── app.lock       # 单实例锁（运行时存在，退出即删）
```

**备份** = 复制整个 `data/` 目录。**升级** = 老版本里 Export（导出 JSON）→ 新版本里 Import（重新嵌入写回），记忆无损迁移。

## 给 agent 用的 REST API

服务只监听 `127.0.0.1`，与老版本完全兼容（`/v1/*`、`/v3/*` 别名齐全）：

```bash
curl http://127.0.0.1:8888/health
curl -X POST http://127.0.0.1:8888/memories \
  -H 'Content-Type: application/json' \
  -d '{"messages":[{"role":"user","content":"我喜欢徒步"}],"user_id":"alice"}'
curl -X POST http://127.0.0.1:8888/search \
  -H 'Content-Type: application/json' \
  -d '{"query":"爱好","filters":{"user_id":"alice"}}'
```

## 记忆分类（Categories）

可选。配置之后，每条新记忆都会被分类模型打上 0..n 个标签，取值来自固定的 15 个预设：`personal_details`、`family`、`professional_details`、`sports`、`travel`、`food`、`music`、`health`、`technology`、`hobbies`、`fashion`、`entertainment`、`milestones`、`user_preferences`、`misc`。标签存在记忆的 payload 里，导出时原样带出，导入时原样保留。

在 **Configuration → 分类 (SystemOne)** 里配置。不配置就完全不分类，记忆的 `categories` 恒为空，且不会报错。

分类是尽力而为的：模型调用失败时记忆照样存下（只是没有标签），失败原因出现在 `/systemone/counts` 的 `last_error`、配置页的「测试」按钮，以及应用日志里。

### 两种决策 API

provider 选 `systemone` 时，还可以选它底下具体用哪家的 **Decision API**：

| | StartLux Decision | Gemini Decision |
|---|---|---|
| 请求地址 | 你填的 `base_url`（**必填**） | `https://generativelanguage.googleapis.com/v1beta/decisions` |
| 鉴权 | `Authorization: Bearer <key>` | `x-goog-api-key: <key>` |
| 默认模型 | `startlux-decision-27b-autothink` | `gemini-decision-1` |
| `noul` 的含义 | 该分类**不**适用的概率 | 该条件**成立**的概率 |
| 命中规则 | `noul < 阈值` | `noul ≥ 阈值` |

两家的 `noul` 方向相反，所以阈值在界面上的说明会跟着切换。Gemini 侧与 Google 文档一致（`noul` 是「条件成立的概率」），实测：一条「杯子到货时是裂的」的消息在「是否到货即损坏？」上得 0.98，一条完好无损的在同一问题上得 0.022。

`misc` 是兜底分类（「其他分类都不适用」），因此只在它是唯一命中项时保留——否则每条记忆都会多出一个 `misc` 标签。

用 Gemini 时只需要填 API 密钥，模型和地址留空即可；也可以填自己的代理地址（会自动补上 `/v1beta/decisions`，重复粘贴完整地址也不会拼两次）。

### 直接调 REST

```bash
# 对一段文本分类
curl -X POST http://127.0.0.1:8888/systemone/classify \
  -H 'Content-Type: application/json' -d '{"text":"我在学 Rust，周末喜欢爬山和吃拉面"}'

# 按标签过滤（AND 语义：必须同时含全部标签）
curl "http://127.0.0.1:8888/memories?categories=food,technology"
```

要给存量的、还没标签的记忆补分类，用界面上的「分类 N 条未分类记忆」按钮，或 `POST /systemone/classify-batch`。

## 排查：添加成功但没存下记忆

`POST /memories` 返回 200 但 `results` 是空数组，说明模型看过这条消息、但没抽出可记忆的内容。这不是错误——闲聊本来就不该产生记忆——但你需要知道是哪一种情况：

| 情况 | 界面显示 |
|---|---|
| 模型回答"没有需要记住的" | 请求页结果数 `0` + 灰标「未抽出」 |
| 模型调用失败（key 错、404、超时） | 灰标「模型调用失败」 |
| 模型回答不是可解析的记忆列表 | 灰标「回答无法解析」 |

鼠标悬停在灰标上会说明具体原因，模型的原始回答和 provider 的报错在 **dashboard → 日志 → 应用日志**（也可直接看 `data/logs/app.log`）。日志页支持按级别过滤、关键字搜索、可选自动刷新（默认关闭）和下载。

日志里的 API key、`Authorization`、`X-API-Key` 等凭证在写入前一律打码；模型回答超过 2000 字符会被截断并标记 `truncated`。

实体页为空是同一件事的另一面：实体是从已存记忆的 `user_id` / `agent_id` / `run_id` 推导出来的，没有记忆自然没有实体。

## 已知限制

- 未签名：Windows SmartScreen / macOS Gatekeeper 首次运行会弹警告，选择"仍要运行"即可（自用 tar.gz 交付不受影响）。
- `--headless` 模式没有窗口和托盘，只能从终端停止（Ctrl-C）。
- 数据目录不要多个进程同时打开（单实例锁会阻止第二个实例启动同一个 `data/`）。
