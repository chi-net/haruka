# AGENTS.md

## 项目

- **haruka**：记账软件，名字取自明日方舟干员「遥」。设计目标：简洁明了，避免复杂功能。
- 用户使用中文交流，回复使用中文。

## 技术栈

- 后端：Rust + axum 0.8 + sea-orm 1（SQLite，sqlx 运行时）
- 前端：服务端渲染，askama 模板（`templates/`）+ htmx（`hx-boost`）+ Tailwind（CDN，无构建步骤）
- 无测试框架、无 lint 配置，验证方式：`cargo build` + 手动跑服务器用 curl 验证

## 命令

- 运行：`cargo run`，监听 `127.0.0.1:3000`（可用 `LISTEN_ADDR` 覆盖）
- 数据库：默认 `sqlite://haruka.db?mode=rwc`（可用 `DATABASE_URL` 覆盖），启动时用 sea-orm `Schema` 自动 `CREATE TABLE IF NOT EXISTS`，无 migration 流程；改表结构后需手动删 `haruka.db`

## 加密（envelope）

- DEK（随机 32 字节）加密数据；KEK 由 Argon2id(密码, salt) 派生并 wrap DEK；`meta` 表存 salt + nonce + wrapped_dek。密码不落盘，改密码只需重 wrap
- 字段级加密：XChaCha20-Poly1305，每条随机 nonce，存 `base64(nonce||ct)` 于原 TEXT 列。加密字段：accounts.name/note、account_details.card_number/account_username、bills.amount/category/note；kind、happened_at、时间戳为明文
- DEK 按客户端会话仅存服务端内存（`AppState.sessions`）；浏览器只保存无过期时间的 `HttpOnly` 随机会话 Cookie，不保存密码或 DEK。Cookie 丢失、主动锁定或服务重启后需重新输入密码；`require_unlock` 中间件把未解锁请求重定向到 `/unlock`（无 meta 时去 `/setup`）
- 密码只用于当次 Argon2id 派生并用 `Zeroizing` 尽快清除，不落盘、不进入 Cookie 或会话；恢复密码会使现有客户端会话全部失效
- 密码恢复使用 BIP-39 英文 12 词助记词；明文助记词只展示一次且不落盘，`recovery` 表只保存恢复密钥包裹 DEK 后的 nonce + ciphertext
- 加解密统一走 `crypto` 模块（`encrypt`/`decrypt_string`/`encrypt_cents`/`decrypt_cents`），handler 里不要直接碰密文
- 加密字段无法 SQL 筛选/排序/求和，汇总统计都在 Rust 里做
- 默认账户在解锁后由 `auth::ensure_default_account` 创建（名称需 DEK 加密，不能在 db 初始化时做）

## 约定

- 金额以**整数分**表示，加密后存于 `bills.amount`（密文），展示经 `handlers::fmt_cents` 格式化；解析表单用 `rust_decimal` 转分。不要用浮点存金额
- 账单类型字段为 `kind`（"income" / "expense"），非 `type`（Rust 关键字）
- 账单时间 `happened_at` 精确到分钟（`NaiveDateTime`），表单用 `datetime-local`，格式 `%Y-%m-%dT%H:%M`
- 删除一律用 POST 表单（`/xxx/{id}/delete`），配合 `hx-confirm` 确认
- 删账户会级联删其账单（FK `on_delete = Cascade`）
- 卡号和账户用户名分开加密存于 `account_details`；列表中卡号只显示后四位，用户名显示前三位和后两位（短用户名需进一步掩码）
- 路由路径参数用 axum 0.8 语法 `{id}`
