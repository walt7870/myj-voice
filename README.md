# 美宜佳语音购物 Rust 服务端

本仓库只保留美宜佳新玩偶项目的 Rust 服务端：设备鉴权、语音 WebSocket、讯飞 IAT/LLM/TTS、订单流程、管理后台和 SQLite 持久化均在 `mjy-voice-shop-rs/` 中实现。

## 目录

```text
mjy-voice-shop-rs/
├── src/                 Rust 服务端源码
├── tests/               集成测试与协议测试
├── static/              浏览器体验页和管理后台静态资源
├── SDKs/                设备文本/音频联调 SDK
├── scripts/             本地开发、验收和部署脚本
├── deploy/              systemd 与 Nginx 配置示例
├── docs/                接口、协议和测试说明
├── Cargo.toml
└── .env.example
```

## 功能概览

- 设备级密钥鉴权和短期 token
- 浏览器文本/语音体验接口
- WebSocket 全双工语音链路，支持动态输入/输出音频 profile
- 讯飞语音听写（IAT）、大模型（LLM）和语音合成（TTS）适配
- 商品匹配、订单创建、订单查询和退单流程
- 管理员登录、设备授权、订单/会话查看和运行时配置查看
- SQLite 数据库自动建表和迁移
- 本地 mock provider，便于无外部凭据开发和测试

## 环境要求

- Rust stable（建议使用最新 stable toolchain）
- Cargo
- SQLite（`sqlx` 使用 SQLite 文件，不要求单独启动数据库服务）
- Node.js 仅在运行浏览器验收脚本时需要

## 本地启动

```bash
cd mjy-voice-shop-rs
cp .env.example .env
```

开发环境可以保持 `MOCK_PROVIDERS=1`，不需要填写讯飞真实凭据。生成管理员密码哈希：

```bash
printf '%s' '请替换为本地管理员密码' \
  | cargo run --bin mjy-admin-password -- hash
```

将命令输出的 Argon2id 字符串写入 `.env` 的 `ADMIN_PASSWORD_HASH`，然后启动：

```bash
cargo run
```

默认监听 `http://127.0.0.1:8787`。健康检查：

```bash
curl http://127.0.0.1:8787/api/health
```

浏览器体验页：

```text
http://127.0.0.1:8787/
```

## 配置项

复制 `.env.example` 后按环境填写。`.env`、数据库、日志和本地密钥均已加入 `.gitignore`，不要提交到仓库。

| 变量 | 说明 |
| --- | --- |
| `HOST` / `PORT` | 监听地址和端口；默认 `127.0.0.1:8787` |
| `DATABASE_URL` | SQLite 地址；默认 `sqlite://mjy_voice_shop.db` |
| `MOCK_PROVIDERS` | 设为 `1` 启用本地讯飞/订单 mock |
| `XF_APP_ID` | 讯飞应用 ID |
| `XF_API_KEY` | 讯飞 API Key |
| `XF_API_SECRET` | 讯飞 API Secret |
| `ADMIN_USERNAME` | 管理员用户名，默认 `myjadmin` |
| `ADMIN_PASSWORD_HASH` | 管理员 Argon2id 密码哈希；公网绑定时必填 |
| `SERVER_SECRET` | 设备 token 签名密钥，生产环境必须使用随机长字符串 |
| `RUST_LOG` | tracing 日志过滤规则，例如 `info,mjy_voice_shop_rs=debug` |

公网部署时必须关闭 mock provider，填写真实讯飞凭据，设置强随机 `SERVER_SECRET` 和管理员密码哈希，并通过 HTTPS/WSS 暴露服务。

## 主要接口

### 健康与能力

```http
GET /api/health
GET /api/device/config
```

`/api/device/config` 返回当前可用的输入/输出音频 profile，客户端应以接口返回值为准，不要硬编码服务端能力矩阵。

### 设备鉴权

```http
POST /api/device/auth
Content-Type: application/json

{"device_id":"<设备 ID>","device_secret":"<设备密钥>"}
```

成功后返回短期 token。设备语音 WebSocket 地址格式：

```text
/api/device/voice?device_id=...&token=...&in_format=...&in_rate=...&out_format=...&out_rate=...
```

四个音频参数在一条连接生命周期内固定；切换格式或采样率应重新握手。

### 浏览器与管理后台

```text
/                    浏览器语音购物体验页
/admin-login.html    管理员登录
/admin.html          管理后台首页
```

管理接口需要管理员会话。设备密钥只在创建/重置时返回一次，生产环境应通过安全渠道保存。

## 测试与构建

在服务目录执行：

```bash
# 全部测试
cargo test

# 发布构建
cargo build --release

# 检查代码格式与 lint（如本机已安装）
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

测试覆盖设备鉴权、音频包边界、WebSocket 生命周期、订单/退单意图、管理员接口和安全访问控制。无真实外部凭据时使用 `.env` 中的 `MOCK_PROVIDERS=1`。

## 部署

`mjy-voice-shop-rs/deploy/` 提供示例配置：

- `mjy-voice-shop-rs.service`：systemd 服务单元
- `mjy-voice-shop-nginx.locations.conf`：Nginx 反向代理 location

部署流程通常为：构建 `target/release/mjy-voice-shop-rs`、准备独立 `.env` 和 SQLite 数据目录、配置 Nginx TLS，再启动 systemd 服务。部署脚本 `scripts/deploy-jd.sh` 仅作为目标环境示例，执行前请审阅路径、用户和凭据策略。

## 安全约定

- 不提交 `.env`、设备密钥、讯飞 API Secret、管理员密码明文、token、私钥、数据库和日志。
- 不在日志中输出 token、设备密钥、讯飞签名 URL 或用户隐私字段。
- 生产设备必须使用一机一密；`DOLL-0001/demo-secret` 仅用于本地开发/回环调试。
- 公网绑定（非 loopback）时必须显式设置 `ADMIN_PASSWORD_HASH`。

## 许可证与内部使用

当前代码为美宜佳新玩偶项目内部服务端实现，具体发布和部署权限以项目约定为准。
