# CPR Vertex AI Adapter v0.2.9

A CPR 3.38.2+ plugin that connects one Google account through the official Google Cloud CLI (gcloud) ADC flow and routes Gemini 4 Argon text requests to Google Vertex AI.

## What it does

- CPR-native Management v2 page: start Google sign-in, display the authorization URL, accept the one-time code, list projects, verify Vertex AI API / Billing, select one project, and disconnect.
- Uses `gcloud auth application-default login --no-launch-browser`; gcloud owns the OAuth exchange, refresh token and access-token refresh. No custom OAuth client, Client Secret, service-account JSON or hand-written token refresh.
- One Google account and one selected project per plugin instance.
- ADC and gcloud config live under the persistent CPR data directory, separate from the Codex accounts and keys.
- Creates one managed OpenAI API-key placeholder account during adapter registration (required by CPR static model-catalog validation), then creates the dedicated `Vertex AI` account group from the management page and adds the account after project verification.
- Gemini 4 Argon text Responses API; all Vertex inference uses `streamGenerateContent?alt=sse`, with downstream `stream:false` assembled into one completed JSON response
- OpenAI `reasoning.effort` maps to Gemini 4 `thinkingLevel` (`low`, `medium`, `high`)
- Uses CPR managed upstream HTTP, account scheduling, usage metering, and the selected account's outbound proxy.

## Runtime dependency

The plugin package does not embed the 94.6 MB Google Cloud CLI archive. Install the official x86_64 Linux archive (which includes bundled Python) into the CPR persistent data directory:

`/app/.runtime/data/cpr-vertex-ai-adapter/google-cloud-sdk/`

The expected executable is:

`/app/.runtime/data/cpr-vertex-ai-adapter/google-cloud-sdk/bin/gcloud`

The 2026-11-03 Google Cloud CLI 592.0.0 archive is documented at <https://docs.cloud.google.com/sdk/docs/install-sdk?hl=en>. Its published SHA-256 is `16fda6058d66e4ec29e8575e2b6e19b0d03cea7b091e6730f7e8361209e6955f`.

## Connect flow

1. Open the Vertex AI page in CPR plugin management.
2. Click **Connect Google account**.
3. Copy the Google authorization URL and open it manually in a separate browser tab, sign in and approve. The CPR plugin iframe intentionally does not navigate to external URLs.
4. Paste the one-time authorization code back into CPR.
5. Select the Cloud Project that owns the billing account / promotional credits.
6. The plugin checks `aiplatform.googleapis.com` and the project's billing status. It does not enable APIs or change billing automatically. For Standard PayGo, Gemini 4 Argon is available in `global`, `us` and `eu` locations.
7. The plugin reuses the registration-time placeholder account and adds it to the `Vertex AI` account group.

The code is sent to the gcloud subprocess stdin and is not stored in plugin state, browser storage or logs. gcloud writes ADC to a private directory (directory mode 0700, ADC file mode 0600). Disconnect removes that plugin-owned ADC directory and removes the managed account from the Vertex group; it does not delete the Google Cloud project or billing account.

## CPR routing setup

The plugin creates one managed OpenAI API-key placeholder account during adapter registration because CPR 3.38.2 requires a non-empty static account scope for every contributed model. The Google ADC remains outside CPR account credentials. The `Vertex AI` account group is created by the management page after it opens, and the connected account is added after project verification.

**Important: configure the upstream-adapter binding as part of installation (or before enabling the instance), not after model-catalog registration.** CPR validates the contributed model catalog against the adapter binding during instance preparation. Configure:

- Stage: `upstream`
- Provider: `openai`
- Models: add this one exact ID (do not use a wildcard):
  - `vertex/google/gemini-4-argon` (GA)
- Account group scope: empty
- Client Key scope: empty
- Failure policy: `reject`

The adapter binding must not use an account-group or Client-Key scope: CPR's static model catalog deliberately rejects those request-specific scopes. Instead, create a normal CPR Client Key and grant that key access to the `Vertex AI` account group. This keeps distribution access control in CPR's native account-group / Client Key mechanism. The plugin does not generate a Client Key secret because the resource-management API does not return a one-time displayable secret.

## Model catalog and supported request scope

The CPR model catalog is static at plugin registration. v0.2.9 exposes exactly one model and declares native reasoning support with `low`, `medium` and `high` effort levels (default `medium`):

- `vertex/google/gemini-4-argon` (GA)

CPR account-level OpenAI model discovery (`GET /v1/models`) is a separate path and is not supported by Vertex AI; a failure there must not be used as an inference health check. The adapter contributes its own static model catalog and sends all inference to the project/location-specific `streamGenerateContent?alt=sse` endpoint, regardless of the downstream stream mode.

Supported request protocol: OpenAI Responses text input/output. Both `stream:false` and `stream:true` are supported. Both modes use Vertex SSE upstream: `stream:false` consumes the full stream and emits one typed `response.completed` `WirePayload::Json` event for JSON delivery, while `stream:true` forwards OpenAI Responses SSE events as text arrives. For explicit `reasoning.effort`, the adapter sets `thinkingConfig.includeThoughts=true`; thought-only frames are accepted as upstream keepalives but their content is never delivered downstream. The model defaults to `MEDIUM`. `temperature`, `top_p` and `top_k` are ignored by Gemini 4 and are not forwarded. The adapter rejects `minimal`, `none`, `xhigh`, tools, images, audio and native `previous_response_id` continuation. `reasoning.summary` accepts the OpenAI values `auto`, `concise` and `detailed` but is ignored because this adapter does not emit a separate reasoning summary.

Gemini 4 uses a unified 1,048,576-token input/output budget. This adapter caps a single non-streaming direct HTTP request at 32,768 output tokens and a streaming request at 65,536 output tokens. Long-decode continuation up to 1,048,576 tokens is not implemented; it requires the `v1beta1` continuation-token flow. The model itself supports additional modalities, but this adapter exposes text-only Responses. Model availability still depends on the selected project's Vertex AI permissions, location and model access.

## Billing and promotional credits

The plugin checks whether Billing is linked, when the Cloud Billing API permission is available. Google does not expose promotional-credit balance and expiry through the Vertex inference API; the management page links to Google Cloud Billing. It does not scrape the console or invent a balance.

## Isolation and security

- The adapter declaration pins exactly one plugin-managed placeholder account through `account_ids` and leaves its model set empty. The upstream binding supplies the Provider and exact model scopes; keep its account-group and Client-Key scopes empty because CPR validates the static model catalog against those scopes. Grant the dedicated `Vertex AI` group to each Client Key that should access Gemini. See [CPR binding and static catalog behavior](docs/design.md).
- The managed placeholder API key is not a Google credential. At inference time the adapter replaces upstream Authorization with a short-lived token obtained by `gcloud auth application-default print-access-token`.
- No Google token or refresh token is stored in CPR account facts or returned to the browser.
- The existing Codex OAuth accounts, Codex Client Keys and Codex provider binding are not modified.
- The plugin is a CPR `trustedProcess`; it is not an OS sandbox. Only install a reviewed build from this repository.

## Build and test

Rust 1.99.1, SDK pinned to CPR v3.38.2 (`021134e4681424edf9882abaca83fb9830d1199f`). The release executable is statically linked with musl so it runs on the CPR Debian 12 container (glibc 2.36) even when the build workspace has a newer glibc:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
rustup target add x86_64-unknown-linux-musl
apt-get install -y musl-tools
CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
cargo build --release --locked --target x86_64-unknown-linux-musl
```

Package the static Linux x86_64 executable with the CPR 3.38.2 `cpr-plugin package` CLI using its supported `x86_64-unknown-linux-gnu` platform label. CPR's package manifest records the OS and architecture; the executable itself is static musl. The target host is Debian 12 / x86_64.
