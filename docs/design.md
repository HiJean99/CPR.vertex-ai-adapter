# 设计与验收边界

## 版本与来源

- CPR 目标宿主：v3.35.0（源码 tag 对应提交 `e633557c50456d200c14f8c84e3a6cbd8cbf6cda`）
- manifest v2、RPC v7、upstream_adapter v5、model_catalog v2
- Google Vertex 使用 `:generateContent`，第一期仅 Gemini 文本 JSON 响应
- ADC 采用 Google 官方 Rust `google-cloud-auth` crate，支持 `external_account` WIF；凭据由运行身份的环境提供，不从 CPR Key 读取
- 第三方 CLIProxyAPI Vertex ADC 插件仅作为研究参考，未拷贝任何代码（该参考仓库未附授权许可证）

## 安全与隔离

- CPR 当前只有 OpenAI / xAI 内置 Provider 可绑定 adapter；本插件挂 OpenAI `api_key` 类型的占位账号
- `accountId` 必须是专用账号，`model_catalog` 必须使用静态 `account_ids=[accountId]`，杜绝目录扩大
- 独立 Client Key 仅赋权 Vertex 账号组；原有 Codex Client Key 不做调整
- 该占位账号必须由管理员手动设置 `model_access=allowlist`，仅包含允许的 `vertex/google/<model>`；否则其他模型可能落入原生 OpenAI 终端，应视为不可启用的隔离缺口
- 适配器只命中精确 `vertex/google/<model>`；上游路径由白名单中的 `<model>` 构造
- 覆盖占位账号的 Authorization，但受管 HTTP 仍保留宿主代理、请求计时、已发送水位、账号健康反馈
- 不将 ADC token 或 Google 凭据写入日志、事件元数据或 CPR 账号凭据
- 插件是 CPR trustedProcess，不具有 OS 沙箱；只能安装经过审查的源码构建产物
- 数据面错误不能在已发送操作后自行重试或改换账号

## 当前功能和缺口

- 已设计：单请求文本输入 → Vertex Gemini generateContent → OpenAI Responses JSON + canonical 用量
- 缺口：SSE、增量消息、工具、thinking、图像、Claude/Anthropic、动态模型目录、项目赠金和额度
- Token 成本由 CPR 按模型手动价格覆盖计算，必须单独配置官方单价；不代表 Google Cloud 实际账单
- 尚无实际 GCP ADC 和 Vertex API 验证，任何编译、协议测试和生产验收都须分别记录，不得互相替代
