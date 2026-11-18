# Quya HTTP ingress with ctx_pool

An OpenAI account explicitly configured with the typed `ctx_pool` mode sends
HTTP generation requests through the existing upstream Responses WebSocket v2
pool. This covers Responses (including passthrough), Chat Completions converted
to Responses, and Anthropic Messages converted to Responses. The client's
streaming SSE or buffered JSON protocol is unchanged.

The policy applies to the account's
`openai_oauth_responses_websockets_v2_mode` or
`openai_apikey_responses_websockets_v2_mode`. Legacy enable flags, default modes,
and the other explicit modes do not opt HTTP clients into this policy.

## Connection failure policy

1. Acquire or connect an upstream WS with a five-second deadline.
2. On connection failure, retry up to three times: four attempts including the
   initial connection, each bounded by five seconds.
3. If all connection attempts fail before sending a generation, use the
   existing HTTP transport and response processing for that request.
4. Once a `response.create` write is attempted, do not replay via HTTP, another
   WS connection, or another account. A write/read failure can have an unknown
   generation outcome and must not cause duplicate execution or charges.

The five-second deadline covers connection acquisition. Generation and response
streaming keep the configured read timeouts.
Explicit `/responses/compact` requests keep their existing HTTP protocol.
Background generation is unsupported by this WS adapter and is rejected before
sending. Payload validation errors do not trigger connection retries.

## Configuration

Enable the global WS feature, WS v2, mode router v2, and the relevant OAuth/API
Key type switch. Set positive account concurrency. Account/global force-HTTP,
an incompatible upstream protocol, or an HTTP transport plugin bound to this
account is a configuration conflict and returns a clear 503 error; it does not
silently bypass the selected mode or plugin.

The existing pool isolates these HTTP connections from native WS ingress and
from other authenticated group/user/API Key/session scopes. Connections also
require matching URL, proxy, credentials and stable handshake headers.
Request tracing IDs may change between turns. Explicit `previous_response_id`
is preserved and uses its known connection when available. No prior response
or truncated input is added automatically.

Existing parsers and billing consume the upstream usage unchanged. Successful
WS forwarding records WS mode and the generation's response ID, not a reused
connection handshake ID. HTTP fallback records HTTP mode. Reused connections
do not replay stale handshake quota headers. Logs identify connection attempts,
actual WS dispatch and HTTP fallback without recording credentials or prompts.

## Validation and test image

`openai_http_ws_transport_test.go` covers pool reuse and identity isolation,
continuation, malformed/error/terminal frames, cancellation and broken-connection
cleanup. `openai_http_ws_policy_test.go` exercises the real HTTP forwarding
entrypoints, both streaming modes, the retry/fallback boundary and replay guard.
The existing native WS pool regression tests run alongside them.

The `Ctx pool test image` workflow builds the pushed feature commit for
`linux/amd64` as `ghcr.io/jxb412/sub2api:ctx-pool-<full-commit>`.
It does not publish a production release tag or replace `latest`.
This feature adds no database migration.
