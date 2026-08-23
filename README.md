# Haruka - 用 AI 和 OCR 解放你的记账体验！

> Haruka 取自《明日方舟》遥干员的英文名。

# 为什么会有 Haruka？

作者曾经是随手记的拥簇，但是随手记太大太冗余太臃肿了，以及最近正好在找记账app，没有一个符合自己要求的记账App，所以就有了轮子再创造 —— Haruka

Haruka 是一个单例软件，理论上它只服务于一个用户。

## Passkey 配置

本地默认使用 `http://localhost:3000` 作为 WebAuthn 来源；需要用这个地址访问（不要改用 `127.0.0.1`）才能注册和登录 Passkey。部署到其他域名时固定配置：

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
