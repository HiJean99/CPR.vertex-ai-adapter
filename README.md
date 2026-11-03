# CPR Vertex AI Adapter (v0.1.1)

独立 Rust 插件，通过 CPR 官方 `upstream_adapter v5` + `model_catalog v2` 挂载 Vertex AI Gemini，不修改 CPR 核心。

## 当前范围

- 第一期：Vertex AI `publishers/google` Gemini **文本、非流式** `POST :generateContent`，OpenAI Responses 协议输入输出
- 采用 Google 官方 Rust `google-cloud-auth` ADC：支持 Service Account、`gcloud auth application-default login` 及 Workload Identity Federation（WIF）配置，不导入真实 Google 凭据到 CPR
- Token 通过 `Authorization: Bearer` 只发送到受控的 Google Vertex AI HTTPS 上游，使用 `x-goog-user-project` 归集配额
- SDK 负责绑定、Key 作用域、账号选择、代理、发送状态、错误观测与结算，插件不直接访问 CPR 数据库
- 目前不支持工具调用、图片、音频、原生续接、WebSocket、流式 SSE、Claude、模型自动发现或 Google 余额查询
- 本仓库是开发工作区；没有接入生产系统，安装启用必须另行批准

## 生产隔离要求

1. 在 CPR 管理端**预先创建**一个仅用于 Vertex AI 的 OpenAI API Key 占位账号；它只能作为适配器账号，不应加入现有 Codex 分组；**必须在账号模型政策中设置 allowlist，仅允许 `vertex/google/<model>` 精确模型 ID**
2. 单独建立 Vertex AI 分组与专用 Client Key，只绑定这个分组，勿将 Vertex 账号加入现有 Codex Key 可见分组
3. 占位 Key 不用于真实认证，插件通过 ADC 替换其 `Authorization` 头；不要复用任何有效 OpenAI Key
4. 将该账号的 `accountId` 与 Vertex 项目 ID、location、显式 Gemini 模型列表配置到插件
5. 为该插件添加唯一 upstream 绑定，限制 `provider=openai`、模型 `vertex/google/*`，故障策略 `reject`；安装时严格核对 Key、账号组与绑定的交集
6. 首次安装仅测试 `/v1/responses` 的 `stream:false` 文本请求；不能拿 Codex CLI 作为当前版本的验收依据

> 插件 SDK 的 `model_catalog v2` 要求注册时就有静态账号 ID，因此此版不能在启用后异步创建账号而直接发布完整模型目录。先使用显式 `accountId`；以后再设计安全的一次性 bootstrap 与重新发布流程。

## ADC 部署前提

在外部 VPS 上优先使用已配置身份提供方的 Workload Identity Federation，具体 `GOOGLE_APPLICATION_CREDENTIALS` 指向由管理员安全保管的 external-account 配置文件；该文件本身不包含服务账号私钥。WIF 仍需要可用的外部身份源，**不能仅靠插件自动获得 Google Cloud 身份**。不支持 WIF 时应暂停生产启用，不应擅自降级到长期 Service Account 私钥。

## 配置示例（无凭据）

```json
{
  "projectId": "example-project-123",
  "quotaProjectId": "example-project-123",
  "location": "global",
  "accountId": "acct_example",
  "models": ["gemini-2.5-flash"]
}
```

公开模型 ID：`vertex/google/gemini-2.5-flash`。Google API 模型 ID：`gemini-2.5-flash`。

## 本地验证

使用 Rust 1.99.1（安装在隔离开发环境中，不要求系统级安装）：

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

`cpr-plugin package` 需要使用兼容 v3.35.0+ 的官方 CLI，包必须构建在最终运行宿主的 OS/架构上。UK 是 ARM64，vm-us 目标通常为 x86_64 Linux；**不能直接把 ARM64 二进制改标签当作生产包**。

参阅 `docs/design.md` 的账号隔离和阶段边界。
