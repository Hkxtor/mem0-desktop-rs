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
- 批量嵌入按 **100 条一块**发（与 provider 自身上限一致），轮换以**块**为单位：一块失败会换 key 重试整块，一块彻底失败时由上层逐条重试。一次 add 抽出 6 条记忆是 1 个批量请求，不是 6 个。

状态是**进程内**的，重启即忘：key 被标作废通常是因为填错或欠费，这两件事都在进程外修好，把它写进磁盘只会让修好的 key 再也回不来。配置页密钥框下面会显示当前状态（`3 把 · 1 把冷却中（47s）· 1 把已作废`），也可以直接问 `GET /keys/status`：

```bash
curl http://127.0.0.1:8888/keys/status
# {"llm":{"keys":3,"cooling":1,"revoked":0,"next_available_in":47,"inherited":false,"last_error":"429 rate limited"},...}
```

接口只报数量与状态，永远不回显密钥本身；`/configure` 里每一把都会被打码成 `[redacted]`（长度保留，所以界面上还知道有几把）。

为什么这样设计、否决了哪些替代方案，见 [`server-rs/docs/adr/0001-api-key-pools.md`](../../docs/adr/0001-api-key-pools.md)。

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

代码里**没有**「重建索引」这种操作，只有「导出 → 清空 → 重新导入」：

```bash
# 1. 先处理待重试队列（reset 会一并清掉）
#    界面：记忆 → 待重试 N
# 2. 导出（上限 1000 条）
curl -s "http://127.0.0.1:PORT/export?format=json" -H "X-API-Key: ..." > mem0-backup.json
# 3. 清空（界面没有这个按钮）
curl -s -X POST "http://127.0.0.1:PORT/reset" -H "X-API-Key: ..."
# 4. 在界面上改 provider / 模型 / 维度 / 输入类型，保存
# 5. 导入，用新模型重新嵌入（保留 id、metadata、时间戳，不重新抽取）
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

## 已知限制

- 未签名：Windows SmartScreen / macOS Gatekeeper 首次运行会弹警告，选择"仍要运行"即可（自用 tar.gz 交付不受影响）。
- `--headless` 模式没有窗口和托盘，只能从终端停止（Ctrl-C）。
- 数据目录不要多个进程同时打开（单实例锁会阻止第二个实例启动同一个 `data/`）。
