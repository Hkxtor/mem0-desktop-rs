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

## 已知限制

- 未签名：Windows SmartScreen / macOS Gatekeeper 首次运行会弹警告，选择"仍要运行"即可（自用 tar.gz 交付不受影响）。
- `--headless` 模式没有窗口和托盘，只能从终端停止（Ctrl-C）。
- 数据目录不要多个进程同时打开（单实例锁会阻止第二个实例启动同一个 `data/`）。
