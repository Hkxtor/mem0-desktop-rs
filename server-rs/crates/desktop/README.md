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

## 多 API 密钥（Key Pool）

每个模型段（`llm` / `embedder` / `systemone`）都可以填**多把同 provider 的密钥**：配置页的密钥框一行一把，最多 20 把。请求在它们之间轮换，而不是只用第一把直到它挂掉。

- **429（限流）**：这把密钥进入冷却，冷却时长取上游的 `Retry-After`，没有就 60 秒；当前这次请求立刻换下一把重试，所以一次限流不会变成一次失败。
- **401 / 403（凭据被拒）**：这把密钥在本进程内作废，不再使用。等待修不好一个错 key。
- **5xx / 网络错误**：不换 key，直接失败。同一个上游对每把 key 都一样坏，轮换只会把一次失败变成三次慢的。
- 全部密钥都不可用时立刻失败，报告里写明最早多久恢复；只有恢复时间 ≤3 秒才会等一下再试。
- `embedder` 和 `systemone` **留空密钥**且 provider 与 `llm` 相同时，自动沿用 LLM 的密钥集合（冷却状态各自独立，因为配额是按模型算的）。

状态是**进程内**的，重启即忘：key 被标作废通常是因为填错或欠费，这两件事都在进程外修好，把它写进磁盘只会让修好的 key 再也回不来。配置页密钥框下面会显示当前状态（`3 把 · 1 把冷却中（47s）· 1 把已作废`），也可以直接问 `GET /keys/status`：

```bash
curl http://127.0.0.1:8888/keys/status
# {"llm":{"keys":3,"cooling":1,"revoked":0,"next_available_in":47,"inherited":false,"last_error":"429 rate limited"},...}
```

接口只报数量与状态，永远不回显密钥本身；`/configure` 里每一把都会被打码成 `[redacted]`（长度保留，所以界面上还知道有几把）。

为什么这样设计、否决了哪些替代方案，见 [`server-rs/docs/adr/0001-api-key-pools.md`](../../docs/adr/0001-api-key-pools.md)。

## 记忆分类（Categories）

可选。配置之后，每条新记忆都会被分类模型打上 0..n 个标签，取值来自固定的 15 个预设：`personal_details`、`family`、`professional_details`、`sports`、`travel`、`food`、`music`、`health`、`technology`、`hobbies`、`fashion`、`entertainment`、`milestones`、`user_preferences`、`misc`。标签存在记忆的 payload 里，导出时原样带出，导入时原样保留。

在 **Configuration → 分类 (SystemOne)** 里配置。不配置就完全不分类，记忆的 `categories` 恒为空，且不会报错。

分类是尽力而为的：模型调用失败时记忆照样存下（只是没有标签），失败原因出现在 `/systemone/counts` 的 `last_error`、配置页的「测试」按钮，以及应用日志里。

### 决策 API

provider 选 `systemone` 表示用 Google 的 **decisions** 接口分类：一次请求、15 个问题、15 个概率，不写 prompt、不解析 JSON 数组。

- 请求地址默认 `https://generativelanguage.googleapis.com/v1beta/decisions`，模型默认 `gemini-decision-1`，都可以留空；填自己的代理地址也行（会自动补上 `/v1beta/decisions`，重复粘贴完整地址不会拼两次）
- 鉴权用 `x-goog-api-key`
- 每个分类问一个 `noul` 问题，返回的 `noul` 是**该条件成立的概率**，达到阈值（默认 0.5）就打上这个标签。与 Google 文档一致，实测：一条「杯子到货时是裂的」的消息在「是否到货即损坏？」上得 0.98，一条完好无损的在同一问题上得 0.022
- 阈值调低打得更多、调高打得更少。实测「我在学 Rust，周末喜欢爬山和吃拉面」在 0.5 下会打 5 个标签，想收紧就调到 0.7 左右

`misc` 是兜底分类（「其他分类都不适用」），因此只在它是唯一命中项时保留——否则每条记忆都会多出一个 `misc` 标签。

> 历史上这里还支持过另一家决策 API。它已经移除：旧配置里的 `vendor` 键会在下一次保存配置时被清掉，连同属于那家服务的 `base_url`（否则记忆正文会被发往一个我们已不再支持的地址）。重新填一把 Google 的密钥即可恢复分类。

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

## 请求页的三个数字

请求页顶部的「操作总数 / 成功率 / 平均耗时」统计的是**整张操作表**（最近 14 天，和日志表一起裁剪），不是下面那张表当前翻到的那 200 条。这一点有过一次真实的错：三个数字原本由页面上取回的行算出来，而列表最多 200 行，于是操作超过 200 次之后三个数字全部冻住，看起来像静态数据。现在它们来自 `GET /requests/stats` 的聚合查询。

## 已知限制

- 未签名：Windows SmartScreen / macOS Gatekeeper 首次运行会弹警告，选择"仍要运行"即可（自用 tar.gz 交付不受影响）。
- `--headless` 模式没有窗口和托盘，只能从终端停止（Ctrl-C）。
- 数据目录不要多个进程同时打开（单实例锁会阻止第二个实例启动同一个 `data/`）。
