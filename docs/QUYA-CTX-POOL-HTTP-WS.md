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

For the ChatGPT OAuth Responses endpoint, a final encoded WS message larger
than 16 MiB uses HTTP before any WS write. This is a conservative compatibility
guard based on the supplied King implementation's reported OAuth limit, not
an independently measured upstream limit. It includes JSON escaping and the
encoder's trailing newline; it does not reduce HTTP ingress limits or impose
this limit on other API Key upstreams. Ordinary connection failures retain the
five-second / three-retry policy. A size guard never retries an already sent
generation.

## Configuration

Enable the global WS feature, WS v2, mode router v2, and the relevant OAuth/API
Key type switch. Set positive account concurrency. Account/global force-HTTP,
an incompatible upstream protocol, or an HTTP transport plugin bound to this
account is a configuration conflict and returns a clear 503 error; it does not
silently bypass the selected mode or plugin.

The existing pool isolates these HTTP connections from native WS ingress and
from other authenticated group/user/API Key/session scopes. Connections also
require matching URL, proxy, credentials and stable handshake headers.
Request tracing IDs and per-turn metadata may change between turns; current
turn metadata is merged into each `response.create.client_metadata` without
changing the original HTTP fallback body. Model routing hints keep the native
pool's advisory affinity. Credentials, cookies, session, fingerprint and
turn-state headers remain part of the strict connection identity.
Explicit `previous_response_id`
is preserved and uses its known connection when available. No prior response
or truncated input is added automatically.

Following the native WS recovery rules, successful HTTP-to-WS turns retain a
bounded, process-local copy of proven complete input and response output. A
continuation can extend that history only when its exact parent response and
authenticated transport scope match. If its original connection is unavailable,
the adapter reconstructs full input, removes the now-unneeded response pointer,
and reconnects before sending the new generation. HTTP fallback after this
recovery also receives the reconstructed full history, preserving other HTTP
request options. It never repeats a generation that may already have been sent.
Successful HTTP fallback responses also extend this history through the
existing terminal-response parsers, so the next turn can recover again.

For OAuth, enabling web search after a connection's first request without
search uses the same recovery before opening a new connection. A known
connection-bound continuation whose history cannot be proved complete receives
an explicit error, rather than silently losing earlier messages. Unknown
response IDs retain the existing upstream resolution behavior.

The history cache holds at most 512 entries / 64 MiB, with a 16 MiB per-entry
budget and a maximum 15-minute TTL. It is not stored in the database or logs.
Process restarts, expiry, or eviction can remove this optional recovery state;
ordinary requests supplying their full context remain independent of it.

Existing parsers and billing consume the upstream usage unchanged. Successful
WS forwarding records WS mode and the generation's response ID, not a reused
connection handshake ID. HTTP fallback records HTTP mode. Reused connections
do not replay stale handshake quota headers. Logs identify connection attempts,
actual WS dispatch and HTTP fallback without recording credentials or prompts.
Dispatch logs include whether a connection was reused. Handshake diagnostics
contain only status, fixed categories, booleans and captured byte counts, never
raw response bodies or credential/header values. A Cloudflare 403 is still an
upstream handshake rejection; improved reuse does not establish its cause.

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

Chinese change rationale and maintenance notes for the 2026-11-19 improvements
are in [QUYA-CHANGES-20261119.md](QUYA-CHANGES-20261119.md).

## GPT-6.2 instruction caching (#8930)

OpenAI documents top-level `instructions` as turn-scoped, uncached input on
GPT-6.2: https://developers.openai.com/api/docs/guides/keep-prefix-unchanged.
For `gpt-6.2-sol` and `gpt-6.2-astra` (including supported reasoning suffixes
and valid dated snapshots), the Codex transform keeps system guidance in its
original input position as developer messages. Gateway-synthesized default
instructions are placed in a leading developer message instead of the top-level
field. Existing explicit top-level instructions remain unchanged.

The Messages forced-instructions template uses the leading developer prefix;
it does not mirror later developer turns into top-level instructions or discard
the client's system prompt when the template omits `ExistingInstructions`.
Continuation requests do not receive another synthetic prefix. GPT-6.1, GPT-5
and other model families, as well as the existing compact path, retain their
prior behavior. This corrects eligibility for prompt caching; cache hits remain
an upstream decision and billing continues to use reported usage.
