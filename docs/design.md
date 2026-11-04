# v0.2.0 design notes

## Scope

One Google user account, one Cloud Project, gcloud ADC, Vertex Gemini text Responses API (JSON + SSE). No custom OAuth application, no WIF, no Service Account key import, no multiple Google accounts, no browser automation and no credits scraping in this release.

## Control plane

- CPR Management v2 routes: `api/status`, `api/login/start`, `api/login/complete`, `api/login/cancel`, `api/project/select`, `api/disconnect`.
- `gcloud auth application-default login --no-launch-browser` runs with a per-plugin `CLOUDSDK_CONFIG` directory. The authorization code is written only to child stdin.
- The login process and its one-time session ID are held in process memory. Restarting the plugin discards pending login sessions; startup removes stale pending directories.
- After gcloud writes an `authorized_user` ADC file, the plugin verifies it by calling `gcloud auth application-default print-access-token`, checks Google userinfo, then atomically promotes the pending config directory to the active gcloud directory.
- Project discovery uses Cloud Resource Manager v1. Project selection checks Service Usage for `aiplatform.googleapis.com` and checks Cloud Billing when permission is available. It never enables APIs or changes Billing.
- Selected project metadata (project ID, location, email, CPR account ID, billing check result) is stored in `project.json` under the plugin persistent data directory. It contains no token or credential.

## CPR integration

- SDK revision pinned to CPR v3.38.2 commit `021134e4681424edf9882abaca83fb9830d1199f`.
- Release binary is statically linked against musl; CPR package metadata identifies the supported `linux/x86_64` runtime target. This avoids depending on the newer glibc version of the ephemeral build image.
- Registration idempotently creates a CPR account group with resource key `vertex-ai`.
- Adapter declaration uses the OpenAI built-in provider, `api_key` authentication kind, base URL `https://aiplatform.googleapis.com/v1/`, `http_sse` transport, empty model/account lists. The binding must restrict it to the `Vertex AI` group and `vertex/google/*` model namespace.
- After Google project verification, the plugin creates or replaces one OpenAI API-key placeholder account and adds it to the group. The provider credential contains only a dummy key and generic Vertex base URL; the Google ADC remains in the gcloud config directory.
- The plugin does not create Client Keys. CPR's `ensure_key` resource API does not return a one-time displayable secret, so a user-facing key is created in the native CPR admin UI.

## Inference path

- Read the plugin-owned project selection and verify the selected CPR account marker.
- Obtain a current ADC access token via gcloud; gcloud handles refresh.
- Use `HostClient::upstream_http` so CPR owns the account proxy, HTTP lifecycle, timeout, send watermark and observation.
- Non-stream: `:generateContent` JSON.
- Stream: `:streamGenerateContent?alt=sse`; parse Vertex SSE incrementally and emit OpenAI Responses SSE frames through a bounded CPR response stream.
- Token usage maps from `usageMetadata`; CPR performs its own model pricing and metering.

## Known limitations

- Static model catalog (four Gemini IDs); refresh requires a new plugin generation.
- Only text Responses API. No Chat Completions input, tools, image/audio, native response continuation or Gemini thinking controls.
- Billing status can be unknown if the user lacks Cloud Billing read permission. Promotional-credit balance and expiry are not available through a supported public inference API.
- No real Google account or paid Vertex request is included in automated tests. Those require an explicit user login and a selected project.
