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
- 数据库：默认 `sqlite://haruka.db?mode=rwc`（可用 `DATABASE_URL` 覆盖），启动时用 sea-orm `Schema` 自动 `CREATE TABLE IF NOT EXISTS`，并由 `db::ensure_column` 用轻量 `ALTER TABLE` 补齐构建阶段新增字段；新增字段时必须同步登记，不能再要求用户手动删除数据库

## 加密（envelope）

- DEK（随机 32 字节）加密数据；KEK 由 Argon2id(密码, salt) 派生并 wrap DEK；`meta` 表存 salt + nonce + wrapped_dek。密码不落盘，改密码只需重 wrap
- 字段级加密：XChaCha20-Poly1305，每条随机 nonce，存 `base64(nonce||ct)` 于原 TEXT 列。加密字段：accounts.name/balance_offset/note、account_details.card_number/account_username/credit_limit、bills.amount/category/note、transfers.amount/note、debt_people.name/note、debt_records.amount/note、categories.name、subscriptions.name/amount/category/note；账户类型、账单日、各记录 kind、账户外键、分类/账单的 is_food、happened_at、订阅周期/到期时间、时间戳为明文
- DEK 按客户端会话仅存服务端内存（`AppState.sessions`）；浏览器只保存无过期时间的 `HttpOnly` 随机会话 Cookie，不保存密码或 DEK。Cookie 丢失、主动锁定或服务重启后需重新输入密码；`require_unlock` 中间件把未解锁请求重定向到 `/unlock`（无 meta 时去 `/setup`）
- setup/unlock/recover 表单禁用 htmx boost，确保会话 Cookie 和重定向走完整浏览器导航；解锁成功直接进入 `/dashboard`
- 密码只用于当次 Argon2id 派生并用 `Zeroizing` 尽快清除，不落盘、不进入 Cookie 或会话；恢复密码会使现有客户端会话全部失效
- 密码恢复使用 BIP-39 英文 12 词助记词；明文助记词只展示一次且不落盘，`recovery` 表只保存恢复密钥包裹 DEK 后的 nonce + ciphertext；在设置页重置助记词必须重新验证主密码
- 加解密统一走 `crypto` 模块（`encrypt`/`decrypt_string`/`encrypt_cents`/`decrypt_cents`），handler 里不要直接碰密文
- 加密字段无法 SQL 筛选/排序/求和，汇总统计都在 Rust 里做
- 默认账户在解锁后由 `auth` 创建；默认收支分类只在首次 setup 时创建（字段需 DEK 加密，不能在 db 初始化时做）

## 约定

- 金额以**整数分**表示，加密后存于 `bills.amount`（密文），展示经 `handlers::fmt_cents` 格式化；解析表单用 `rust_decimal` 转分。不要用浮点存金额
- 账单类型字段为 `kind`（"income" / "expense"），非 `type`（Rust 关键字）
- 账单时间 `happened_at` 精确到分钟（`NaiveDateTime`），表单用 `datetime-local`，格式 `%Y-%m-%dT%H:%M`
- 删除一律用 POST 表单（`/xxx/{id}/delete`），配合 `hx-confirm` 确认
- 删账户会级联删其账单、转账和借还记录（FK `on_delete = Cascade`）；删借贷对象会级联删其借还记录
- 卡号和账户用户名分开加密存于 `account_details`；列表中卡号只显示后四位，用户名显示前三位和后两位（短用户名需进一步掩码）
- 账户类型直接存于 `accounts.kind`：payment（支付，用户名）、bank（银行，卡号）、stored_value（储值卡，卡号）、credit_card（信用卡，卡号 + 授信额 + 账单日）、credit_service（信贷服务，用户名 + 授信额 + 账单日）、investment（投资）、other（其他）；切换类型需清除不适用的账户详情
- 授信额以整数分加密存于 `account_details.credit_limit`，账单日明文存于 `billing_day`（1..=31）；账单的账户下拉和列表需附带掩码后的卡号或用户名
- 信用卡和信贷服务的余额不得低于负授信额；普通支出、订阅支出、转账、借还、删除记录、修改授信额和强制余额等所有可能降低余额的入口都必须在写库前校验，不能绕过额度
- 强制设置余额不创建账单；将“目标余额 - 当前资金记录净额”以整数分加密存入 `accounts.balance_offset`。账户余额为 offset 加账单、转账和借还记录的资金净额
- 转账单独存于 `transfers`，转出账户扣款、转入账户加款，不计入普通收入/支出汇总
- 借还记录 kind：lend（我借给对方，账户扣款）、borrow（我向对方借入，账户加款）、repayment_received（对方还我，账户加款）、repayment_paid（我还对方，账户扣款）；借贷对象单独存于 `debt_people`
- 收支分类由设置页 CRUD，存于 `categories`；新建或编辑账单只能选择对应 income/expense 类型下仍存在的分类，删除分类不改写历史账单
- 支出分类可标记 `is_food`；创建或编辑账单时将标记快照存入 `bills.is_food`，保证分类后续改名或删除不改变历史恩格尔系数。恩格尔系数按本月食品支出 / 本月总支出计算
- 订阅服务存于 `subscriptions`，包含服务名、每次金额、支出分类、周期（day/week/month/quarter/year）、到期时间和备注；订阅看板按到期时间展示状态，一键支出时由用户选择账户并生成普通 expense 账单，成功后以到期时间和当前时间中较晚者为基准自动顺延一个周期；订阅仍可手动删除
- `/dashboard` 是默认资金看板，集中账户概览、转账、借还、近期资金动态及日/周/月/年收支图表；`/transfers`、`/debts` 的 GET 只重定向到看板，POST 操作完成后也返回看板。借贷对象仍由 `/debt-people` 独立管理
- 所有 HTTP 500 响应由统一中间件渲染为客户端可见的错误页，必须显示原始错误详情并同时写入服务端日志，禁止空白页或吞掉错误
- 路由路径参数用 axum 0.8 语法 `{id}`
