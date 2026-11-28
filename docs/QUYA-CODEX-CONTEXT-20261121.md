# Codex browser and terminal context compatibility — 2026-11-21

Codex Desktop can include browser tabs as `agent.browser_tabs` in a standalone
`function_call_output` without a `call_id` (upstream issue #8999). Terminal context
uses the same shape with `agent.terminal_info` (#8991). The official normalization
added by #8622, #8769 and #8929 did not recognize these names, so an HTTP Responses
request could fail local validation with `function_call_output requires call_id`
before selecting an upstream account. The affected code matched official main;
the earlier fixes had not been lost during the Quya merge.

This change extends both the fast candidate filter and content validation to the
two exact names. Nonblank string outputs become user `input_text`, preserving the
content and order using the existing `<external_NAME>` wrapper. This follows the
official compatibility strategy and works with Responses, Chat Completions and
Anthropic protocol conversion; it does not claim native untrusted-tool semantics.

Real nonblank call IDs, malformed IDs, wrong namespaces, unknown names, non-text
or blank outputs, and duplicate JSON fields retain their previous behavior.
Unpaired same-name tool calls are treated as ambiguous and left unchanged;
complete tool history may coexist with newly injected context. Existing context
types and the HTTP tool validation rules are unchanged. No database or pricing
changes are required.

Regression coverage includes the original HTTP 400 cases, content preservation,
Unicode-escaped names, strict matching, malformed inputs, real and ambiguous tool
history, idempotence, large numeric metadata, and Chat/Anthropic conversion. The
image workflow runs the Codex context regressions before publishing the image.

Deployment scope: <TEST_SERVER>:<TEST_HTTP_PORT> test instance only. Production promotion
is deferred as requested. Deployment and live verification evidence is retained
outside the repository under the task artifacts directory; credentials must not
be committed.
