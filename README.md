# Haruka - 用 AI 和 OCR 解放你的记账体验！

> Haruka 取自《明日方舟》遥干员的英文名。

# 为什么会有 Haruka？

作者曾经是随手记的拥簇，但是随手记太大太冗余太臃肿了，以及最近正好在找记账app，没有一个符合自己要求的记账App，所以就有了轮子再创造 —— Haruka

Haruka 是一个单例软件，理论上它只服务于一个用户。

## 本地运行

需要 Rust stable、Node.js 24 和 npm。Ubuntu/Debian 从源码构建还需要 C 编译工具及 OpenSSL 开发文件：

```sh
sudo apt-get update
sudo apt-get install build-essential pkg-config libssl-dev
```

首次运行先安装前端构建依赖并生成本地 CSS：

```sh
npm ci
npm run css:build
cargo run
```

Tailwind 样式由 `assets/tailwind.css` 扫描模板和 Rust 源码后编译到 `static/app.css`，运行时不依赖 Tailwind CDN。开发样式时可使用 `npm run css:watch`。

## GitHub Actions 构建

仓库内的 `.github/workflows/build.yml` 会在 push、Pull Request 或手动触发时执行以下检查：

1. 使用 `npm ci` 安装锁定的前端依赖并重新生成 Tailwind CSS；
2. 检查生成的 `static/app.css` 是否已经提交；
3. 执行 `cargo fmt --check` 和 `cargo build --locked --release`；
4. 上传保留 14 天的 `haruka-linux-x86_64` 构建产物。

从 GitHub Actions 页面下载并解压 `haruka-linux-x86_64.tar.gz` 后即可取得在 Ubuntu 24.04 x86_64 上构建的可执行文件。Tailwind CSS 已嵌入二进制，部署机器不需要安装 Node.js，也不需要额外复制 `templates/` 或 `static/`。其他操作系统或较旧的 Linux 发行版建议按照“本地运行”一节从源码构建。

## 部署

### 监听地址和端口

不传配置时 haruka 只监听 `127.0.0.1:3000`。配置优先级为：命令行参数、`LISTEN_ADDR`、`PORT`、默认值。

| 配置 | 行为 |
| --- | --- |
| `--listen 127.0.0.1:8080` | 精确设置监听地址和端口，适合放在同机反向代理后面 |
| `--port 8080` | 监听 `0.0.0.0:8080` |
| `LISTEN_ADDR=0.0.0.0:8080` | 通过环境变量精确设置监听地址和端口 |
| `PORT=8080` | 监听 `0.0.0.0:8080`，适合自动注入 `PORT` 的部署平台 |

直接运行二进制时：

```sh
./haruka --listen 127.0.0.1:8080
./haruka --port 8080
PORT=8080 ./haruka
```

从源码运行时，需要用 `--` 把参数传给 haruka：

```sh
cargo run -- --port 8080
```

`--port` 和 `PORT` 会监听所有网络接口。若不需要外部设备直接访问，建议使用 `--listen 127.0.0.1:端口`，再通过 HTTPS 反向代理提供服务。

### 数据库和持久化

默认数据库是当前工作目录下的 `haruka.db`。生产环境建议把数据库放在持久化目录，并确保运行用户拥有该目录的写权限：

```sh
DATABASE_URL='sqlite:///var/lib/haruka/haruka.db?mode=rwc' \
./haruka --listen 127.0.0.1:3000
```

持久化或备份时需要保留 `haruka.db`，以及 SQLite 运行期间可能出现的 `haruka.db-wal`、`haruka.db-shm`。容器部署时应把整个数据库目录挂载到持久卷，而不是只挂载单个文件。

服务器还需要能够通过 HTTPS 访问 `api.frankfurter.dev` 获取汇率。网络不可用时会回退到数据库中的历史汇率缓存；某个货币对完全没有缓存时，相关操作会向客户端明确报错。

### HTTPS、反向代理和 Passkey

生产环境的 Passkey 必须使用 HTTPS，并且公开来源和 RP ID 在首次注册后应保持不变：

```sh
DATABASE_URL='sqlite:///var/lib/haruka/haruka.db?mode=rwc' \
PASSKEY_ORIGIN='https://haruka.example.com' \
PASSKEY_RP_ID='haruka.example.com' \
./haruka --listen 127.0.0.1:3000
```

例如使用 Caddy 终止 TLS：

```caddyfile
haruka.example.com {
    reverse_proxy 127.0.0.1:3000
}
```

`PASSKEY_ORIGIN` 必须是用户在浏览器中实际访问的完整 HTTPS 来源，`PASSKEY_RP_ID` 只填写域名。切换域名、协议或 RP ID 后，原有 Passkey 将无法继续使用。

### systemd 示例

假设二进制位于 `/opt/haruka/haruka`，数据库目录为 `/var/lib/haruka`：

```ini
[Unit]
Description=haruka accounting service
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=haruka
Group=haruka
WorkingDirectory=/var/lib/haruka
ExecStart=/opt/haruka/haruka --listen 127.0.0.1:3000
Environment="DATABASE_URL=sqlite:///var/lib/haruka/haruka.db?mode=rwc"
Environment="PASSKEY_ORIGIN=https://haruka.example.com"
Environment="PASSKEY_RP_ID=haruka.example.com"
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
```

修改后执行 `systemctl daemon-reload`，再使用 `systemctl enable --now haruka` 启动服务。

## Passkey 配置

本地默认使用当前监听端口对应的 `http://localhost:<端口>` 作为 WebAuthn 来源；需要用这个地址访问（不要改用 `127.0.0.1`）才能注册和登录 Passkey。部署到其他域名时固定配置：

```sh
PASSKEY_ORIGIN=https://haruka.example.com PASSKEY_RP_ID=haruka.example.com cargo run
```

生产环境必须使用 HTTPS。已有 Passkey 与来源和 RP ID 绑定，后续不要随意更改这两个值。

# 预计的 Features

- 内置订阅管理，妈妈再也不用担心我忘了续费啦！
- 恩格尔系数看板（闲着没事写上去的哈哈哈）
- 自动化 OCR 设计，你只需要确认
- 可选的AI Endpoint（AI传输内容不过服务端，你怎么设置怎么来，你完全可以使用本地的 ollama 来进行回复！）
- 自带 iCloud Shortcuts，你甚至可以直接截图然后自动记账（截图目前预计支持微信/支付宝/四大加一招行）
- 货币支持（CNY/HKD/USD，汇率随时变动）
- 可能的ETF，持仓等分析

# 授权和AI声明

本项目使用 vibe coding 技术强力驱动并使用 MIT 授权协议，你想怎么用就怎么用去。
