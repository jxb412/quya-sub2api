package service

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"strings"
	"sync/atomic"

	"github.com/tidwall/gjson"
)

// 2026-11-19：WS 握手失败后 HTTP 成功也必须延续完整历史，否则下一轮使用
// HTTP 子响应 ID 时会丢上下文。只在既有解析点读取成功终态，不包装/复制响应流。
type openAIHTTPWSHTTPReplayContextKey struct{}

type openAIHTTPWSHTTPReplay struct {
	plan      *openAIHTTPWSReplayPlan
	completed atomic.Pointer[openAIHTTPWSHTTPCompleted]
	// 2026-11-19：仅记输出是否出现，不缓存 delta；终态空数组不能覆盖已出现的正文。
	outputObserved atomic.Bool
}

type openAIHTTPWSHTTPCompleted struct {
	responseID string
	output     []json.RawMessage
}

func (s *OpenAIGatewayService) prepareOpenAIHTTPWSHTTPHistory(req *http.Request, proxyURL string, account *Account, attempt *openAIHTTPWSAttempt) *http.Request {
	if attempt == nil || !attempt.fallback.Load() || attempt.sent.Load() {
		return req
	}
	// 每次 HTTP 兼容重试重新记录实际请求，禁止沿用前次失败响应或清理前的 input。
	attempt.httpReplay.Store(nil)
	if req == nil || req.GetBody == nil || req.ContentLength > openAIHTTPWSReplayMaxEntryBytes {
		return req
	}
	identity, err := openAIHTTPWSHistoryIdentity(req, proxyURL, account, attempt)
	if err != nil {
		return req
	}
	reader, err := req.GetBody()
	if err != nil {
		return req
	}
	body, err := io.ReadAll(io.LimitReader(reader, openAIHTTPWSReplayMaxEntryBytes+1))
	_ = reader.Close()
	if err != nil || int64(len(body)) > openAIHTTPWSReplayMaxEntryBytes {
		return req
	}
	plan := s.prepareOpenAIHTTPWSReplayPlan(identity, body, account)
	if !plan.complete || openAIHTTPWSHTTPHistoryBytes(plan.input) > openAIHTTPWSReplayMaxEntryBytes {
		return req
	}
	replay := &openAIHTTPWSHTTPReplay{plan: plan}
	attempt.httpReplay.Store(replay)
	return req.WithContext(context.WithValue(req.Context(), openAIHTTPWSHTTPReplayContextKey{}, replay))
}

func openAIHTTPWSHTTPHistoryBytes(items []json.RawMessage) int64 {
	bytes := openAIHTTPWSReplayEntryOverhead
	for _, item := range items {
		bytes += int64(len(item)) + openAIHTTPWSReplayItemOverhead
	}
	return bytes
}

func observeOpenAIHTTPWSHTTPHistory(resp *http.Response, payload []byte, eventType string) {
	if resp == nil || resp.Request == nil {
		return
	}
	attempt := openAIHTTPWSAttemptFromContext(resp.Request.Context())
	if attempt == nil || !attempt.fallback.Load() || attempt.sent.Load() {
		return
	}
	replay, _ := resp.Request.Context().Value(openAIHTTPWSHTTPReplayContextKey{}).(*openAIHTTPWSHTTPReplay)
	if replay == nil || replay.plan == nil || !replay.plan.complete {
		return
	}
	eventType = strings.TrimSpace(eventType)
	if eventType == "" {
		eventType = strings.TrimSpace(gjson.GetBytes(payload, "type").String())
	}
	if openAIHTTPWSHasOutputEvent(eventType) {
		replay.outputObserved.Store(true)
	}
	if eventType != "" && eventType != "response.completed" && eventType != "response.done" {
		if isOpenAIWSTerminalEvent(eventType) || eventType == "error" {
			replay.completed.Store(nil)
		}
		return
	}
	// 2026-11-19：SSE 的 [DONE] 或额外 usage 块不是响应对象，不能清除刚收到的成功终态。
	if eventType == "" && !gjson.GetBytes(payload, "status").Exists() {
		return
	}
	// 不累计 delta。只有合法成功终态的完整 output 数组可以作为恢复证据。
	replay.completed.Store(nil)
	if int64(len(payload)) > openAIHTTPWSReplayMaxEntryBytes || !json.Valid(payload) {
		return
	}
	response := gjson.ParseBytes(payload)
	if eventType != "" {
		response = response.Get("response")
	}
	if !response.IsObject() || response.Get("status").String() != "completed" {
		return
	}
	id := strings.TrimSpace(response.Get("id").String())
	output := response.Get("output")
	if id == "" || !output.IsArray() || (response.Get("error").Exists() && response.Get("error").Type != gjson.Null) {
		return
	}
	var items []json.RawMessage
	if json.Unmarshal([]byte(output.Raw), &items) != nil {
		return
	}
	if len(items) == 0 && replay.outputObserved.Load() {
		return
	}
	for _, item := range items {
		value := gjson.ParseBytes(item)
		if !value.IsObject() || value.Get("type").String() == "" || value.Get("type").String() == "item_reference" {
			return
		}
	}
	if openAIHTTPWSHTTPHistoryBytes(replay.plan.input)+openAIHTTPWSHTTPHistoryBytes(items)+int64(len(id)+len(replay.plan.identity)) > openAIHTTPWSReplayMaxEntryBytes {
		return
	}
	replay.completed.Store(&openAIHTTPWSHTTPCompleted{responseID: id, output: items})
}

// 2026-11-19：共享 HTTP/WS 的输出证据标记，防止把有 delta 的空终态当完整历史。
func openAIHTTPWSHasOutputEvent(eventType string) bool {
	return strings.HasPrefix(eventType, "response.output_") ||
		strings.HasPrefix(eventType, "response.reasoning_") ||
		strings.HasPrefix(eventType, "response.function_call_") ||
		strings.HasPrefix(eventType, "response.custom_tool_call_")
}

func (s *OpenAIGatewayService) completeOpenAIHTTPWSHTTPHistory(ctx context.Context, attempt *openAIHTTPWSAttempt, result *OpenAIForwardResult, err error) {
	// 2026-11-19：解析成功仍可能随后断流或被取消，必须等整次转发无错才入库。
	if err != nil || result == nil || ctx.Err() != nil || attempt == nil || !attempt.fallback.Load() || attempt.sent.Load() {
		return
	}
	replay := attempt.httpReplay.Load()
	if replay == nil || replay.plan == nil || !replay.plan.complete {
		return
	}
	completed := replay.completed.Load()
	if completed == nil || (result.ResponseID != "" && result.ResponseID != completed.responseID) {
		return
	}
	s.getOpenAIHTTPWSReplayStore().Put(replay.plan.identity, completed.responseID,
		combineOpenAIWSReplayItems(replay.plan.input, completed.output), s.openAIWSResponseStickyTTL())
}
