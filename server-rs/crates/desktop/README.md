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

## 界面：没有账户页

桌面应用只有一位操作者，没有「个人资料」可改。侧边栏的「设置」页已移除，它的三个部分各有归宿：

| 原来在设置里的 | 现在在哪 |
|---|---|
| 浅色 / 深色 / 跟随系统 | **配置 → 外观**（页面最底部，跟服务端、桌面设置在一起） |
| 名字 / 邮箱 | 不再需要，已删除 |
| 修改密码 | 不再需要，已删除 |

主题存在浏览器里，点开就生效，不用按「保存配置」；重启应用也还在。`/dashboard/settings/` 不再存在，用户菜单里的「设置」入口改指向「配置」。

鉴权默认关闭（服务只听 `127.0.0.1`），所以本来也没有人需要改密码；显式设 `AUTH_DISABLED=false` 才需要登录，那时「退出登录」仍在右上角的用户菜单里。

## 多 API 密钥（Key Pool）

每个模型段（`llm` / `embedder` / `systemone`）都可以填**多把同 provider 的密钥**：配置页的密钥框一行一把，最多 20 把。请求在它们之间轮换，而不是只用第一把直到它挂掉。

- **429（限流）**：这把密钥进入冷却，冷却时长取上游的 `Retry-After`，没有就 60 秒；当前这次请求立刻换下一把重试，所以一次限流不会变成一次失败。
- **401 / 403（凭据被拒）**：这把密钥在本进程内作废，不再使用。等待修不好一个错 key。
- **5xx / 网络错误**：不换 key，直接失败。同一个上游对每把 key 都一样坏，轮换只会把一次失败变成三次慢的。
- 全部密钥都不可用时立刻失败，报告里写明最早多久恢复；只有恢复时间 ≤3 秒才会等一下再试。
- `embedder` 和 `systemone` **留空密钥**且 provider 与 `llm` 相同时，自动沿用 LLM 的密钥集合（冷却状态各自独立，因为配额是按模型算的）。
- 批量嵌入按 **100 条一块**发（与 provider 自身上限一致），轮换以**块**为单位：一块失败会换 key 重试整块，一块彻底失败时由上层逐条重试。一次 add 抽出 6 条记忆是 1 个批量请求，不是 6 个。

出错时会带上密钥的**后 3 位**（`…a1b`）：池里五把 key 的时候，"所有密钥都被限流"这句话没法让人去改配置，而 3 位字符足够认出是哪一把、又不足以还原。短于 20 字符的 key 不显示后 3 位（那是它的很大一部分）。完整密钥永远不会出现在任何消息里。

状态是**进程内**的，重启即忘：key 被标作废通常是因为填错或欠费，这两件事都在进程外修好，把它写进磁盘只会让修好的 key 再也回不来。配置页密钥框下面会显示当前状态（`3 把 · 1 把冷却中（47s）· 1 把已作废`），也可以直接问 `GET /keys/status`：

```bash
curl http://127.0.0.1:8888/keys/status
# {"llm":{"keys":3,"cooling":1,"revoked":0,"next_available_in":47,"inherited":false,"last_error":"429 rate limited"},...}
```

接口只报数量与状态，永远不回显密钥本身；`/configure` 里每一把都会被打码成 `[redacted]`（长度保留，所以界面上还知道有几把）。

为什么这样设计、否决了哪些替代方案，见 [`server-rs/docs/adr/0001-api-key-pools.md`](../../docs/adr/0001-api-key-pools.md)。

## LLM 模型与思考强度

抽取记忆用的模型在 **Configuration → LLM 模型** 里配置：provider、模型、密钥、Base URL，以及**思考强度**（`reasoning_effort`）。

思考强度只对推理模型有意义（`o1` / `o3` / `gpt-5` 这几个系列）。选项是 `minimal`、`low`、`medium`、`high`，默认是**不发送**——请求体里连这个键都没有，用上游模型自己的默认档。不做成默认值是有意的：对一个没配过这一项的人，升级之后发出去的请求必须与升级前一模一样。

- 模型名不被识别为推理模型时（`gpt-4o`、`claude`、本地模型……）这一项即使存着也不会发送。客户端只在认得出的推理模型上带这个参数——往不认识的端点塞未知字段，通常是 400 而不是被忽略。
- 抽取记忆是「照着原文摘出事实」，不需要深度推理。模型响应慢时先调低这一项再看看，比换模型便宜得多。
- 分类器（SystemOne）有自己的预算（4096 tokens），不受这一项影响。

顺手记下另外几个默认值，它们目前只能在 `POST /configure` 的 `llm.config` 里手工指定：`temperature` 0.1、`top_p` 0.1、`max_tokens` 8192（见 `crates/mem0-llm/src/config.rs`）。

## 嵌入模型（Embedding）
在 **Configuration → 嵌入模型** 里配置：provider、模型、密钥、Base URL、维度，以及（少数模型才需要的）**输入类型**。

### 输入类型（`input_type`）

有些模型是**非对称**的：写进库里的句子和用来检索的问句要用不同的方式编码。NVIDIA 的 `nemotron-3.5-embed-*` 就属于这一类，而且不带这个字段会**直接 400**（`'input_type' parameter is required for asymmetric models`）。

反过来，OpenAI 官方端点对不认识的请求字段会 400（`Unrecognized request argument supplied`），所以这个字段**默认不发送**：

| 选择 | 请求体里出现什么 |
|---|---|
| 不发送（默认） | 没有 `input_type` 字段，任何 provider 都接受 |
| `auto` | 存记忆时发 `passage`，检索时发 `query`，改写（update）按 `passage` |
| `query` / `passage` | 无论做什么都发这个值 |

填错值（比如 `sideways`）会在保存时被拒，并说明只接受 `auto` / `query` / `passage`——不会静默变成「什么都不发」。把选择改回「不发送」并保存，配置里的键会被删除，而不是留一个空字符串。

> 给已经装满记忆的库**打开**这个开关，等于换了一套编码空间：旧的向量仍然按 passage 存、新的按 query 存，检索质量会下降。要么接受，要么按下面的流程重建一遍。

### 换嵌入模型的流程

代码里**没有**「重建索引」这种操作，只有「导出 → 清空 → 重新导入」。四步都在界面上：

1. 「记忆」页先处理**待重试队列**（清空会一并清掉它）。
2. 「导出」页点**导出全部记忆**——按页读完整库，不受分页影响。
3. 「配置」页底部**危险操作 → 清空所有记忆**，输入 `RESET` 确认；它会先下载一份完整备份，
   备份成功才清空。
4. 改 provider / 模型 / 维度 / 输入类型并保存，然后回到「导出」页**导入 JSON 导出文件**，
   用新模型重新嵌入（保留 id、metadata、时间戳，不重新抽取）。

想用 curl 也可以（`/export` 支持 `offset` / `limit`，`limit` 上限 1000）：

```bash
curl -s "http://127.0.0.1:PORT/export?format=json&offset=0&limit=1000" -H "X-API-Key: ..."
# 用返回的 total_in_store 判断要不要继续翻页，truncated 为 true 就接着取
curl -s -X POST "http://127.0.0.1:PORT/reset" -H "X-API-Key: ..."
curl -s -X POST "http://127.0.0.1:PORT/import" -H "X-API-Key: ..." \
     -H "Content-Type: application/json" -d @mem0-backup.json
```

维度变了而没清空时，保存会被拒并提示 `holds N vectors at width A but config says B; export and re-import`——表是空的就会自动按新宽度重建，所以全新安装随便改。

**维度必须填对**：它不只是记账，会作为 `dimensions`（OpenAI 兼容）或 `outputDimensionality`（Gemini）真的发给上游。留空则用模型默认宽度（OpenAI 1536、Gemini 768），而库里表的宽度沿用旧值，于是第一次写入就 `dimension mismatch`。

### NVIDIA NIM（`integrate.api.nvidia.com`）

provider 选 `openai`（OpenAI 兼容的 `/v1/embeddings`），Base URL 填 `https://integrate.api.nvidia.com/v1`。实测（2026-11，一个普通 nvapi 密钥）：

| 模型 | 维度 | 需要 `input_type` | 单次耗时（中位 / p90） |
|---|---|---|---|
| `nvidia/nemotron-3.5-embed-1b` | 128–2048 任意整数（默认 2048） | 是 | 0.81s / 0.88s |
| `nvidia/nemotron-3-embed-1b` | 只能 2048 | 否 | 0.45s / 0.57s |
| `nvidia/nemotron-3.5-embed-8b` | 1–4096 任意整数（默认 4096） | 是 | 0.76s / 1.19s |

三个都在亚秒级，维度对耗时几乎没有影响（8B 在 1024 / 3072 / 4096 上分别是 0.73 / 0.76 / 0.96s）。8B 并发 8 个 3072 维请求也是一秒左右全部返回。所以选哪个是**质量与体积**的取舍，不是速度问题：3072 维比 1024 维的库大约三倍。

> 一次早期测量里 8B 的中位是 23.9 秒、并发还会撞上游 120 秒网关超时（504）。同一把密钥过一阵复测就全是亚秒级——那是上游冷启动/容量问题，不是模型固有属性。遇到长耗时先重试几次再下结论。

`nvidia/embed-qa-4`、`nv-embedqa-mistral-7b-v2`、`snowflake/arctic-embed-l` 等旧模型在 `/v1/models` 里列着，但对普通密钥返回 `404 Function … Not found for account`。

## 记忆分类（Categories）

可选。配置之后，每条新记忆都会被分类模型打上 0..n 个标签，取值来自固定的 15 个预设：`personal_details`、`family`、`professional_details`、`sports`、`travel`、`food`、`music`、`health`、`technology`、`hobbies`、`fashion`、`entertainment`、`milestones`、`user_preferences`、`misc`。标签存在记忆的 payload 里，导出时原样带出，导入时原样保留。

在 **Configuration → 分类 (SystemOne)** 里配置。不配置就完全不分类，记忆的 `categories` 恒为空，且不会报错。

分类是尽力而为的：模型调用失败时记忆照样存下（只是没有标签），失败原因出现在 `/systemone/counts` 的 `last_error`、配置页的「测试」按钮，以及应用日志里。

### 分类在后台补，add 不等它

`POST /memories` 的响应里 `categories` 恒为 `[]`，标签在记忆存下之后由后台 worker 逐条补上：

- **为什么要这样**：分类只影响记忆页和分类页，add 的调用方从不读它。同步分类让一次实测 11017 ms 的 add 多花 1383 ms，还和抽取抢同一批密钥配额。
- **一条一条来**：worker 是单线程串行的。一次 add 抽出 6 条记忆，标签是依次出现的，不是同时——刻意的，避免刚用完配额的提取之后立刻并发 6 个模型调用。
- **看到标签的时间差**：正常情况下几百毫秒到几秒（分类模型本身的延迟）。刚 add 完立刻刷新记忆页，可能看到一行没有标签，再刷一次就有了。
- **重启会丢**：队列在内存里，进程退出时还没分类的记忆不会被补上，会一直算作「未分类」。点一次「分类 N 条未分类记忆」即可补完，或调 `POST /systemone/classify-batch`。
- **失败不重试**：某条分类失败只留空标签，不会在后台反复重试烧配额。原因见 `last_error`，记忆计数在 `/systemone/counts` 的 `unclassified`。

`/systemone/counts` 因此多了两个字段：`pending`（还在队列里等分类的条数）和 `classified`（本次进程启动以来后台补上的条数）。刚 add 完看到 `unclassified` 增加是正常的，等 `pending` 回到 0 再看。

请求页（Requests）里 ADD 行的耗时条仍然有四段，但「分类」段恒为 0——那段时间已经不在请求里了。

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

| 情况 | 界面显示 | 原文是否保留 |
|---|---|---|
| 模型回答"没有需要记住的" | 请求页结果数 `0` + 灰标「未抽出」 | 不需要重试 |
| 模型调用失败（key 错、限速、5xx、超时） | 灰标「模型调用失败」 | **保留**，可重试 |
| 模型回答不是可解析的记忆列表（一条都没救回） | 灰标「回答无法解析」 | **保留**，可重试 |
| 记忆抽到了、但拿不到向量 | 灰标「嵌入失败」 | **保留**，可重试 |
| 存储环节出错 | 灰标「添加失败」 | **保留**，可重试 |

保留了原文的失败，Requests 页那行会多一个黄色「已暂存」标记，点进去直接打开重试抽屉。

鼠标悬停在灰标上会说明具体原因，模型的原始回答和 provider 的报错在 **dashboard → 日志 → 应用日志**（也可直接看 `data/logs/app.log`）。

「回答无法解析」最常见的原因是**回答被输出长度上限截断**：模型正在写那段 JSON 的时候预算用完了，数组没有闭合。日志里会写明 `json_error`（`EOF while parsing a string` 就是截断，`expected ',' or '}'` 才是真的格式错）、`answer_chars` 和 `answer_tail`（回答的结尾，能直接看到断在哪里）。

这种情况下**已经写完的条目会被保留**：解析失败后会逐个扫描 `"memory"` 数组里的对象，把完整的存下来，只丢掉被切断的最后一条，日志里记成 `stage: salvaged`（"extraction answer was incomplete; kept the complete entries"），Requests 页那行显示实际存下的条数。只有一条都没写完时才会出现「回答无法解析」灰标。

输出预算是 `llm.config.max_tokens`，默认 8192。思考型模型（Gemini 3.x 这类）的思考 token 也算在这个预算里，所以答案看起来不长也可能被截断；库很大、一次要抽几十条记忆时可以再调高。日志页支持按级别过滤、关键字搜索、可选自动刷新（默认关闭）和下载。

日志里的 API key、`Authorization`、`X-API-Key` 等凭证在写入前一律打码；模型回答超过 2000 字符会被截断并标记 `truncated`。

实体页为空是同一件事的另一面：实体是从已存记忆的 `user_id` / `agent_id` / `run_id` 推导出来的，没有记忆自然没有实体。

### 失败添加的重试队列

模型调用失败、回答无法解析、嵌入失败这三类，问题多半在上游、会自己好——但客户端那条请求已经关了，它没法把原文再发一次。所以这些失败会把素材留下：

- **保留什么**：原始 `messages`，以及失败当时已经抽出的条目文本（嵌入失败才有）。
- **在哪**：`data/vectors.db` 的 `failed_adds` 表；界面在 **记忆 → 待重试 N → 抽屉**，逐条列时间、scope、原文摘要、最后一次报错和尝试次数，可以单条重试、全部重试、全部删除。原文默认只显示前 200 字，可展开全文。
- **重试做什么**：有条目就只补嵌入、按原文本入库（不会重新提问模型），并把原始对话写进这条记忆的「输入」面板；没有条目就带着当前配置重新跑一次抽取——这正是不必重新配置就能恢复的原因。
- **成功后**：记录立刻删除，原文此后由这条记忆的 `memory_inputs` 承载，不再存第二份。日志里记一条 `resolved` 并带上新 memory id。
- **重试也不算失败的情况**：那条文本已经是一条记忆了（重试前会先查同 scope 下有没有同样的文本，避免插出重复条目），或者重新抽取后模型认为没什么可记——这两种都不会产生新记忆，但再试一次只会得到同样的结果，所以记录一样删掉。
- **去重**：同一段 `messages` + 同一组 scope 只留一条，重复失败累加 `attempts`。客户端在限速期间重发五十次，抽屉里也只有一行。
- **边界**：保留 14 天、最多 500 条（超了淘汰最旧的），随日志裁剪一起执行；`/reset` 会一并清空。
- **接口**：`GET /memories/failed`、`POST /memories/failed/retry`（`{"ids": [...]}` 单条，或 `{"limit": 20}` 取最旧一批，连败 3 次即停）、`DELETE /memories/failed`。都要 admin。

为什么这样设计（尤其"失败反而把用户原文存下来、成功后立刻删掉"这两条看着反直觉的决定），见 `server-rs/docs/adr/0002-failed-add-retry.md`。

## 图标

`icons/` 里的图标是 mem0 的品牌标记（取自 dashboard 的 wordmark），放在应用的深色底上：

| 文件 | 用途 |
|---|---|
| `icon.ico` | Windows 可执行文件资源、任务栏、标题栏（16/24/32/48/64/128/256 七个尺寸） |
| `icon-256.png` / `icon-128.png` | Tauri 打包与窗口/托盘默认图标 |
| `icon.png` | 32×32 兜底 |
| `icon.svg` / `icon-small.svg` | 源文件：完整标记 / 16–24px 用的简化标记 |

16 和 24 像素用的是**简化版**（中心圆 + 一圈 8 个点）：完整标记有二十多个小点，缩到 16px 只剩一团噪点。两者共用同一底色与轮廓，所以看起来是同一个图标。

重新生成（需要 google-chrome 与 ImageMagick）：

```bash
cd server-rs/crates/desktop/icons
for s in 32 48 64 128 256; do
  google-chrome --headless --disable-gpu --no-sandbox \
    --default-background-color=00000000 --window-size=$s,$s --hide-scrollbars \
    --screenshot=f$s.png file://$PWD/icon.svg
done
for s in 16 24; do
  google-chrome --headless --disable-gpu --no-sandbox \
    --default-background-color=00000000 --window-size=$s,$s --hide-scrollbars \
    --screenshot=f$s.png file://$PWD/icon-small.svg
done
convert f256.png f128.png f64.png f48.png f32.png f24.png f16.png icon.ico
cp f256.png icon-256.png; cp f128.png icon-128.png; cp f32.png icon.png
rm -f f16.png f24.png f32.png f48.png f64.png f128.png f256.png
```

`--default-background-color=00000000` 是必须的：少了它圆角外会填成白色。

## 导出、清空与重导入（换嵌入模型的完整流程）

换嵌入模型或维度时，旧的向量和新模型不在同一个向量空间里，唯一不丢文本的路径是
**导出 → 清空 → 改配置 → 导入**。现在这四步都在界面上。

### 导出：一次取全库

「导出」页的「导出全部记忆」会按页把整个库读完再生成文件，不受列表分页影响。
`GET /export` 因此多了三个字段：

```json
{
  "total": 1000,
  "total_in_store": 1200,
  "offset": 0,
  "truncated": true,
  "memories": ["..."]
}
```

- `total` 是**本次响应里的条数**（历史上就是这个含义，没有改）。
- `total_in_store` 是**库里的总条数**。
- `truncated` 表示后面还有。

`limit` 默认 1000，超过 1000 会被压到 1000；`limit=0` 是一个合法的空页，只用来看
`total_in_store`（配置页危险操作卡片的计数就是这么来的）。

不再提供 CSV / Pydantic 选项：服务端三种格式一直返回的都是 JSON，界面上的这两个按钮只会
产出后缀是 `.csv` / `.py` 的 JSON 文件。只有 JSON 能原样导入回来，所以现在只导出 JSON。

### 清空：先备份，再清空

「配置」页底部的「危险操作 → 清空所有记忆」需要输入 `RESET` 才能点。确认后会：

1. 按页读完整个库，下载一份 `mem0-memories-YYYYMMDD-HHMM.json`；
2. 备份成功后才调 `POST /reset`；
3. 备份取不到、或者取到的条数与弹窗里显示的条数不一致（期间有人增删），**中止，不清空**。

顺序是这条路径的全部意义：先清空再备份不叫备份。`POST /reset` 会一并清掉
`memory_inputs` 和待重试队列，所以重置前先看一眼「记忆」页的待重试列表。

### 导入

「导出」页的「导入 JSON 导出文件」会用**当前**配置重新嵌入每条记忆，不会重新抽取，
id / metadata / 时间戳都保留。

> 导出的每一页都在浏览器里攒着，最后一次性写成文件；库特别大时（数十万条）会占不少内存。
> 真要处理那种规模，应该改成流式写盘，那是另一件事。

## 请求页：点开一行看明细和耗时

「请求」页每行只说「结果数 3」，看不出是哪三条；耗时只有一个总数，也看不出慢在哪。
现在**点任意一行**，行下方会展开两块内容，再点一次收起（同时只展开一行）：

**结果明细**——这次操作到底存了什么、搜到了什么：

| 操作 | 明细里有什么 |
|---|---|
| `ADD` | 存下来的每条记忆（每条截断到 80 字） |
| `SEARCH` | 命中的每条记忆 + 相似度（三位小数） |
| `UPDATE` / `DELETE` / `GET` | 被操作的那条记忆 |
| `IMPORT` / `RESET` | 没有明细：批量操作的条数已经在「结果数」里说清了 |

超过 20 条只留前 20 条并标注「前 20 条 / 共 N 条」。每条右侧的「查看」跳到「记忆」页。
明细随 `memory_operations` 一起 14 天后清理，不单独长期保留。

**耗时分布**——一条分段条，把总耗时拆成四段：

| 阶段 | 含义 | 慢的时候该做什么 |
|---|---|---|
| 模型抽取 | 调用 LLM 把对话抽成记忆 | 换更快的模型，或看「上游限速」徽标 |
| 嵌入 | 调嵌入模型把文本变成向量 | 换更快的嵌入模型（见「嵌入模型」一节） |
| 分类 | 给每条记忆打分类标签 | 分类是可选的，关掉就没这一段 |
| 其他 | 写向量库、写历史、中间等待 | 一般很小；大了说明是本地库的问题 |

读、删这类不调模型的操作没有分阶段耗时（本来就没有模型调用），只显示总数。

分段耗时只统计**这一次操作自己**的时间：两个请求同时在跑也不会互相加总。
失败添加的重试队列走的是另一条路径，不在这套计时范围内，所以那些行没有分段耗时。

## 上游失败了，到底是什么错

`/requests` 每行除了「结果数」和失败原因，还多一个上游失败分类的徽标，鼠标悬停给出建议：

| 徽标 | `error_kind` | 触发 | 建议 |
|---|---|---|---|
| 上游限速 | `rate_limited` | 429 | 等一会儿，或给这一段多填几把 key |
| 密钥被拒 | `auth_failed` | 401 / 403 | 检查这一段的 API 密钥 |
| 模型不存在 | `model_not_found` | 404 | 核对模型名与 Base URL |
| 上游故障 | `upstream_error` | 5xx，以及其余 4xx | 上游的问题，稍后重试 |
| 连不上上游 | `network_error` | 没有拿到任何响应 | 检查网络、代理、Base URL |

分类只依据**上游 HTTP 状态码**，不匹配错误文案——同一个 429 每个厂商写得都不一样。
模型正常回答「没什么可记的」（`{"memory": []}`）**不算失败**，所以没有分类徽标。

400 被归到「上游故障」而不是「模型不存在」：模型名拼错确实常报 400，但不是只有这一种原因，
猜错会把用户支去改一个本来没问题的模型名。具体原因看「日志」页的应用日志。

## 已知限制

- 未签名：Windows SmartScreen / macOS Gatekeeper 首次运行会弹警告，选择"仍要运行"即可（自用 tar.gz 交付不受影响）。
- `--headless` 模式没有窗口和托盘，只能从终端停止（Ctrl-C）。
- 数据目录不要多个进程同时打开（单实例锁会阻止第二个实例启动同一个 `data/`）。
