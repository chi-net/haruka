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
- 首次启动（accounts 为空时）自动创建「默认账户」

## 约定

- 金额以**整数分**（`i64`）存储在 `bills.amount`，展示经 `handlers::fmt_cents` 格式化；解析表单用 `rust_decimal` 转分。不要用浮点存金额
- 账单类型字段为 `kind`（"income" / "expense"），非 `type`（Rust 关键字）
- 账单时间 `happened_at` 精确到分钟（`NaiveDateTime`），表单用 `datetime-local`，格式 `%Y-%m-%dT%H:%M`
- 删除一律用 POST 表单（`/xxx/{id}/delete`），配合 `hx-confirm` 确认
- 删账户会级联删其账单（FK `on_delete = Cascade`）
- 路由路径参数用 axum 0.8 语法 `{id}`
