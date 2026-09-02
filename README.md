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

首次运行先安装前端构建依赖并生成本地浏览器资源：

```sh
npm ci
npm run web:build
cargo run
```

Tailwind 样式由 `assets/tailwind.css` 扫描模板和 Rust 源码后编译到 `static/app.css`，开发样式时可使用 `npm run css:watch`。`web:build` 还会复制锁定版本的 htmx、Chart.js、Tesseract.js Web Worker/WASM 和简体中文、英文 OCR 模型；生成文件提交在 `static/` 并嵌入二进制，运行时不依赖第三方 CDN。

浏览器会注册一个轻量 Service Worker，仅缓存编译后的 CSS 和锁定版本的非敏感前端运行文件。包含账户、账单等解密内容的 HTML/JSON 不会进入离线缓存，任何 AJAX 或账务写入也不会离线排队；断网操作会明确失败，恢复网络后不会自动重放。

## 浏览器票据识别

快速记账的“扫描票据”在浏览器 Web Worker 中运行 Tesseract.js WASM，图片不会上传至 haruka 后端。首次识别需要从 haruka 自身加载约 18 MB 的本地 OCR 运行文件和中英文模型，之后语言数据由 Tesseract.js 缓存在浏览器 IndexedDB 中。

识别后的文字可以用本地规则提取金额和时间，也可以由浏览器直接调用用户填写的 OpenAI Chat Completions 兼容完整 URL。URL 与模型只保存在当前浏览器 localStorage，API Key 不持久保存、刷新页面即清除；后端不会代理或看到 URL、密钥、图片及 OCR 文字。自定义 AI 服务必须允许 haruka 当前来源进行 CORS 请求，否则浏览器会明确报告跨域失败。所有识别结果都只填入可编辑快速记账草稿，仍需用户确认后才会写入账单。

## 每日定投

“定投”页面可创建每日基金定投计划。计划固定绑定一个非信用扣款账户和一个“投资账户”，两端必须使用相同货币，并可设置手续费率，例如 `0.15%`。每个中国大陆交易日会把本金以普通转账转入投资账户；实际手续费按本金乘费率计算并四舍五入到分，额外从扣款账户扣除，单独生成“投资手续费”支出。这样本金不会被误算成消费，而手续费会正常进入支出统计。

haruka 按北京时间判断交易日：周六、周日不执行，并排除上海证券交易所公告的休市日。程序内置 2025、2026 年官方休市安排；后续年份可在定投页面的“交易日历校准”中补充工作日休市日期。

由于数据库 DEK 只存在于已解锁的内存会话，服务锁定或重启后不能在后台无人值守地解密定投金额。期间遗漏的交易日会保留，用户解锁并访问定投页后由联网 AJAX 幂等补执行；同一计划同一交易日最多生成一次流水。余额不足时计划会保持待执行并显示原因，不会静默跳过。

投资账户不维护基金代码、份额或每日净值，也不依赖行情 API。建议每月从基金平台查看一次当前持仓总价值，然后在投资账户详情中使用“校准持仓价值”：收益增加余额、亏损减少余额，差额作为不可删除的余额调整流水保留，但不计入普通收入或支出。

## GitHub Actions 构建

仓库内有两条构建工作流：

- `.github/workflows/build.yml` 会在 push、Pull Request 或手动触发时构建 Linux 二进制；
- `.github/workflows/container.yml` 会构建 `linux/amd64` 和 `linux/arm64` 镜像。Pull Request 只验证镜像能够构建，推送到 `main` 或推送 `v*` 标签时才发布到 GHCR。

二进制工作流会执行以下检查：

1. 使用 `npm ci` 安装锁定的前端依赖并重新生成浏览器资源；
2. 检查生成的 `static/` 产物是否已经提交；
3. 执行 `cargo fmt --check` 和 `cargo build --locked --release`；
4. 上传保留 14 天的 `haruka-linux-x86_64` 构建产物。

从 GitHub Actions 页面下载并解压 `haruka-linux-x86_64.tar.gz` 后即可取得在 Ubuntu 24.04 x86_64 上构建的可执行文件。CSS、前端运行库和 OCR 模型均已嵌入二进制，部署机器不需要安装 Node.js，也不需要额外复制 `templates/` 或 `static/`。其他操作系统或较旧的 Linux 发行版建议按照“本地运行”一节从源码构建。

容器工作流使用仓库自带的 `GITHUB_TOKEN` 发布 `ghcr.io/<仓库所有者>/haruka`，不需要额外配置镜像仓库密码。`main` 对应 `latest` 和 `main` 标签，版本标签（例如 `v0.2.0`）会发布 `0.2.0`、`0.2` 等标签，同时每次构建还会生成提交 SHA 标签。首次发布后可在 GitHub Packages 设置中将镜像改为 Public；如果保持 Private，部署机器需要先使用具有 `read:packages` 权限的令牌执行 `docker login ghcr.io`。

## 部署

### 使用 GHCR 镜像部署

镜像默认监听容器内的 `0.0.0.0:3000`，默认把 SQLite 数据库放在 `/data/haruka.db`。下面使用 Docker 命名卷保存整个数据库目录，并只把服务发布到宿主机回环地址，适合在 Caddy、Nginx 等 HTTPS 反向代理后运行：

如需先在本机从当前源码构建镜像，可执行 `docker build --tag haruka:local .`；多阶段 Dockerfile 会在构建阶段重新生成 Tailwind CSS 和 release 二进制，最终镜像不包含 Node.js、Rust 工具链或源码。

```sh
export HARUKA_IMAGE='ghcr.io/YOUR_GITHUB_OWNER/haruka:latest'

docker volume create haruka-data
docker pull "$HARUKA_IMAGE"
docker run -d \
  --name haruka \
  --restart unless-stopped \
  --publish 127.0.0.1:3000:3000 \
  --mount type=volume,src=haruka-data,dst=/data \
  --env PORT=3000 \
  --env PASSKEY_ORIGIN='https://haruka.example.com' \
  --env PASSKEY_RP_ID='haruka.example.com' \
  "$HARUKA_IMAGE"
```

将 `YOUR_GITHUB_OWNER` 替换为发布镜像的 GitHub 用户名或组织名，并把 Passkey 两项改成实际对外域名。升级时保留同一个命名卷并重建容器：

```sh
docker pull "$HARUKA_IMAGE"
docker rm --force haruka
docker run -d \
  --name haruka \
  --restart unless-stopped \
  --publish 127.0.0.1:3000:3000 \
  --mount type=volume,src=haruka-data,dst=/data \
  --env PORT=3000 \
  --env PASSKEY_ORIGIN='https://haruka.example.com' \
  --env PASSKEY_RP_ID='haruka.example.com' \
  "$HARUKA_IMAGE"
```

也可以使用 Compose：

```yaml
services:
  haruka:
    image: ghcr.io/YOUR_GITHUB_OWNER/haruka:latest
    restart: unless-stopped
    ports:
      - "127.0.0.1:3000:3000"
    environment:
      PORT: "3000"
      PASSKEY_ORIGIN: https://haruka.example.com
      PASSKEY_RP_ID: haruka.example.com
    volumes:
      - haruka-data:/data

volumes:
  haruka-data:
```

保存为 `compose.yaml` 后执行 `docker compose up -d`。镜像已经设置 `DATABASE_URL=sqlite:///data/haruka.db?mode=rwc`；如需改用其他容器内目录或文件名，可在 `environment` 中覆盖。备份时应备份整个 `haruka-data` 卷，因为 SQLite 运行时可能同时存在 WAL 和 SHM 文件。

如果要直接向局域网发布而不使用同机反向代理，可把端口映射改成 `3000:3000`。Passkey 在非 localhost 环境仍然需要 HTTPS，不能仅靠开放 HTTP 端口使用。

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

Firefox/macOS 的原生 Passkey 与安全密钥路径曾存在 PRF 输出不一致问题。haruka 注册时会保持创建与确认使用同一种认证器路径；如果已有凭据在 Firefox 返回了不同于注册时的 PRF，登录页会要求输入一次主密码，为该凭据增加当前浏览器的兼容包裹。原有 Safari 或其他浏览器的包裹会保留，不需要删除 Passkey。外部安全密钥用户仍建议使用已包含相关修复的最新 Firefox。

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
