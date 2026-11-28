package service

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"sort"
	"strings"
	"sync"

	"github.com/Wei-Shaw/sub2api/internal/pkg/apicompat"
	"github.com/gin-gonic/gin"
	"github.com/tidwall/gjson"
)

// 2026-11-28: the API-key opt-in keeps the client's synchronous contract while
// requesting upstream SSE. Buffer within the existing response limit, preserve
// actual usage, and estimate absent usage through the existing local tokenizer.
// Incomplete streams retain text but never expose unfinished executable tools.
func (s *OpenAIGatewayService) bufferSyncUpstreamChatJSON(c *gin.Context, resp *http.Response, account *Account, requestBody []byte, model string) error {
	body, err := ReadUpstreamResponseBody(resp.Body, s.cfg, c, openAITooLargeError)
	if err != nil {
		if !errors.Is(err, ErrUpstreamResponseBodyTooLarge) {
			writeChatCompletionsError(c, http.StatusBadGateway, "upstream_error", "Failed to read upstream response")
		}
		return fmt.Errorf("read synchronous upstream chat stream: %w", err)
	}
	response, incomplete, observed, err := aggregateSyncUpstreamChat(body, requestBody, model)
	if err != nil {
		writeSyncUpstreamIncompleteError(c)
		return fmt.Errorf("aggregate synchronous upstream chat stream: %w", err)
	}
	usage, _ := extractOpenAIUsageFromJSONBytes(response)
	if incomplete {
		recordOpenAINonStreamingIncomplete(resp, c, account, false, string(body), "", &usage, true)
		MarkOpsStreamErrorValue(c, OpsStreamError{ErrType: "upstream_error", Code: openAINonStreamingIncompleteCode,
			Message: "Upstream stream ended before its final response; available output recovered", IntendedStatus: http.StatusBadGateway, NonStream: true})
	} else if !observed {
		// Only fixed metadata and numeric usage enter diagnostics; no customer
		// notice, prompt text, or private estimator field is added to the JSON.
		detail, _ := json.Marshal(gin.H{"reason": "missing_usage", "usage_source": "local_estimate", "billed_usage": usage})
		appendOpsUpstreamError(c, OpsUpstreamErrorEvent{Platform: account.Platform, AccountID: account.ID, AccountName: account.Name,
			UpstreamStatusCode: resp.StatusCode, UpstreamRequestID: resp.Header.Get("x-request-id"), Kind: "protocol_error",
			Reason: "upstream_missing_usage", Message: "Upstream completion usage missing; local token estimate applied", Detail: string(detail)})
	}
	// Close the network body now; the existing JSON billing/response paths own
	// the replacement body and keep their original model/price/group behavior.
	_ = resp.Body.Close()
	resp.Body = io.NopCloser(bytes.NewReader(response))
	resp.Header.Set("Content-Type", "application/json; charset=utf-8")
	resp.Header.Del("Content-Length")
	resp.ContentLength = int64(len(response))
	return nil
}

func writeSyncUpstreamIncompleteError(c *gin.Context) {
	MarkResponseCommitted(c)
	c.JSON(http.StatusBadGateway, gin.H{"error": gin.H{"type": "upstream_error", "code": openAINonStreamingIncompleteCode, "message": "Upstream stream ended without usable completion output"}})
}

type syncChatChoice struct {
	content, refusal, reasoningContent, reasoning strings.Builder
	tools                                         map[int]*apicompat.ChatToolCall
	legacy                                        *apicompat.ChatFunctionCall
	finish                                        string
}

func aggregateSyncUpstreamChat(body, requestBody []byte, model string) ([]byte, bool, bool, error) {
	var result map[string]any
	observed, incomplete := false, false
	if !bodyHasSSEFraming(body) {
		if !gjson.ValidBytes(body) || !gjson.GetBytes(body, "choices").IsArray() || gjson.GetBytes(body, "error").Exists() {
			return nil, false, false, errors.New("invalid upstream chat JSON")
		}
		if err := json.Unmarshal(body, &result); err != nil {
			return nil, false, false, err
		}
		observed = syncChatUsageObserved(gjson.GetBytes(body, "usage"))
	} else {
		result = map[string]any{"object": "chat.completion", "model": model}
		choices := map[int]*syncChatChoice{}
		var parseErr error
		forEachOpenAISSEFrame(string(body), func(event string, data []byte) {
			if parseErr != nil {
				return
			}
			root := gjson.ParseBytes(data)
			if !gjson.ValidBytes(data) || !root.IsObject() || root.Get("error").Exists() || event == "error" {
				parseErr = errors.New("invalid/error upstream chat stream event")
				return
			}
			for _, key := range []string{"id", "created", "model", "system_fingerprint", "service_tier"} {
				if v := root.Get(key); v.Exists() {
					result[key] = v.Value()
				}
			}
			if u := root.Get("usage"); syncChatUsageObserved(u) {
				result["usage"] = u.Value()
				observed = true
			}
			for _, deltaChoice := range root.Get("choices").Array() {
				i := deltaChoice.Get("index")
				if i.Type != gjson.Number || i.Int() < 0 || i.Float() != float64(i.Int()) {
					parseErr = errors.New("invalid choice index")
					return
				}
				index := int(i.Int())
				choice := choices[index]
				if choice == nil {
					choice = &syncChatChoice{tools: map[int]*apicompat.ChatToolCall{}}
					choices[index] = choice
				}
				delta := deltaChoice.Get("delta")
				for key, dst := range map[string]*strings.Builder{"content": &choice.content, "refusal": &choice.refusal, "reasoning_content": &choice.reasoningContent, "reasoning": &choice.reasoning} {
					v := delta.Get(key)
					if v.Type == gjson.String {
						_, _ = dst.WriteString(v.Str)
					} else if v.Exists() && v.Type != gjson.Null {
						parseErr = errors.New("unsupported chat delta content")
						return
					}
				}
				if f := deltaChoice.Get("finish_reason"); f.Type == gjson.String {
					choice.finish = f.Str
				}
				for _, tool := range delta.Get("tool_calls").Array() {
					ti := tool.Get("index")
					if ti.Type != gjson.Number || ti.Int() < 0 || ti.Float() != float64(ti.Int()) {
						parseErr = errors.New("invalid tool index")
						return
					}
					index := int(ti.Int())
					target := choice.tools[index]
					if target == nil {
						target = &apicompat.ChatToolCall{Type: "function"}
						choice.tools[index] = target
					}
					target.ID += tool.Get("id").String()
					if typ := tool.Get("type").String(); typ != "" {
						target.Type = typ
					}
					target.Function.Name += tool.Get("function.name").String()
					target.Function.Arguments += tool.Get("function.arguments").String()
				}
				if fn := delta.Get("function_call"); fn.IsObject() {
					if choice.legacy == nil {
						choice.legacy = &apicompat.ChatFunctionCall{}
					}
					choice.legacy.Name += fn.Get("name").String()
					choice.legacy.Arguments += fn.Get("arguments").String()
				}
			}
		})
		if parseErr != nil {
			return nil, false, observed, parseErr
		}
		indices := make([]int, 0, len(choices))
		for i := range choices {
			indices = append(indices, i)
		}
		sort.Ints(indices)
		out := make([]apicompat.ChatChoice, 0, len(indices))
		for _, i := range indices {
			choice := choices[i]
			message := apicompat.ChatMessage{Role: "assistant", Refusal: choice.refusal.String(), ReasoningContent: choice.reasoningContent.String(), Reasoning: choice.reasoning.String()}
			if choice.content.Len() > 0 {
				message.Content, _ = json.Marshal(choice.content.String())
			} else {
				message.Content = json.RawMessage("null")
			}
			finished := choice.finish == "stop" || choice.finish == "tool_calls" || choice.finish == "function_call"
			invalidTools := false
			toolIndices := make([]int, 0, len(choice.tools))
			for i := range choice.tools {
				toolIndices = append(toolIndices, i)
			}
			sort.Ints(toolIndices)
			for _, ti := range toolIndices {
				tool := choice.tools[ti]
				if finished && tool.Type == "function" && tool.ID != "" && syncChatCompleteFunction(tool.Function) {
					message.ToolCalls = append(message.ToolCalls, *tool)
				} else {
					incomplete = true
					invalidTools = true
					choice.finish = "length"
				}
			}
			if choice.legacy != nil {
				if finished && syncChatCompleteFunction(*choice.legacy) {
					message.FunctionCall = choice.legacy
				} else {
					incomplete = true
					invalidTools = true
					choice.finish = "length"
				}
			}
			if invalidTools {
				// Do not let a client execute only part of a tool-call batch.
				message.ToolCalls, message.FunctionCall = nil, nil
			}
			if choice.finish == "" {
				incomplete = true
				choice.finish = "length"
			}
			out = append(out, apicompat.ChatChoice{Index: i, Message: message, FinishReason: choice.finish})
		}
		result["choices"] = out
	}
	encoded, err := json.Marshal(result)
	if err != nil {
		return nil, incomplete, observed, err
	}
	usable := false
	for _, choice := range gjson.GetBytes(encoded, "choices").Array() {
		msg := choice.Get("message")
		if msg.Get("content").Type == gjson.String && msg.Get("content").Str != "" || msg.Get("refusal").String() != "" || len(msg.Get("tool_calls").Array()) > 0 || msg.Get("function_call.name").String() != "" {
			usable = true
		}
	}
	if !usable {
		return nil, incomplete, observed, errors.New("no usable completion output")
	}
	if !observed {
		usage, err := estimateSyncUpstreamChatUsage(requestBody, encoded, model)
		if err != nil {
			return nil, incomplete, false, err
		}
		result["usage"] = map[string]int{"prompt_tokens": usage.InputTokens, "completion_tokens": usage.OutputTokens, "total_tokens": usage.InputTokens + usage.OutputTokens}
		encoded, err = json.Marshal(result)
		return encoded, incomplete, false, err
	}
	return encoded, incomplete, true, nil
}

func syncChatUsageObserved(u gjson.Result) bool {
	return u.Get("prompt_tokens").Type == gjson.Number || u.Get("completion_tokens").Type == gjson.Number || u.Get("input_tokens").Type == gjson.Number || u.Get("output_tokens").Type == gjson.Number
}

func syncChatCompleteFunction(fn apicompat.ChatFunctionCall) bool {
	return strings.TrimSpace(fn.Name) != "" && gjson.Valid(fn.Arguments) && gjson.Parse(fn.Arguments).IsObject()
}

// An empty usage object on a final event must not erase valid usage received
// earlier. Explicit numeric zero remains authoritative, including on finals.
func lastSyncOpenAIResponsesUsage(body []byte) (OpenAIUsage, *apicompat.ResponsesUsage) {
	var usage OpenAIUsage
	var reported *apicompat.ResponsesUsage
	forEachOpenAISSEFrame(string(body), func(_ string, data []byte) {
		for _, path := range []string{"usage", "response.usage", "data.usage", "data.response.usage"} {
			value := gjson.GetBytes(data, path)
			if !syncChatUsageObserved(value) {
				continue
			}
			var decoded apicompat.ResponsesUsage
			actual, ok := openAIUsageFromGJSON(value)
			if ok && json.Unmarshal([]byte(value.Raw), &decoded) == nil {
				usage, reported = actual, &decoded
			}
		}
	})
	return usage, reported
}

func estimateSyncUpstreamChatUsage(requestBody, response []byte, model string) (OpenAIUsage, error) {
	var req apicompat.ChatCompletionsRequest
	if err := json.Unmarshal(requestBody, &req); err != nil {
		return OpenAIUsage{}, err
	}
	responsesReq, err := apicompat.ChatCompletionsToResponsesWithPromptPolicy(&req, 0, true)
	if err != nil {
		return OpenAIUsage{}, err
	}
	input, err := json.Marshal(responsesReq)
	if err != nil {
		return OpenAIUsage{}, err
	}
	var output []any
	for _, choice := range gjson.GetBytes(response, "choices").Array() {
		msg := choice.Get("message")
		output = append(output, gin.H{"type": "message", "content": []any{gin.H{"type": "output_text", "text": msg.Get("content").String()}, gin.H{"type": "refusal", "refusal": msg.Get("refusal").String()}}})
		for _, key := range []string{"reasoning", "reasoning_content"} {
			if value := msg.Get(key).String(); value != "" {
				output = append(output, gin.H{"type": "reasoning", "summary": []any{gin.H{"type": "summary_text", "text": value}}})
			}
		}
		for _, tool := range msg.Get("tool_calls").Array() {
			output = append(output, gin.H{"type": "function_call", "name": tool.Get("function.name").String(), "arguments": tool.Get("function.arguments").String()})
		}
		if fn := msg.Get("function_call"); fn.Exists() {
			output = append(output, gin.H{"type": "function_call", "name": fn.Get("name").String(), "arguments": fn.Get("arguments").String()})
		}
	}
	encodedOutput, err := json.Marshal(output)
	if err != nil {
		return OpenAIUsage{}, err
	}
	return estimateOpenAINonStreamingRecoveredUsage(input, encodedOutput, model)
}

// The shared Responses reader scans on another goroutine. Capture only the
// configured byte limit and snapshot under a mutex so terminal early returns
// cannot race with the scanner. Overflow disables recovery rather than billing
// an arbitrarily truncated prompt/response.
type syncOpenAIBoundedCapture struct {
	mu       sync.Mutex
	buf      bytes.Buffer
	limit    int64
	overflow bool
}

func (w *syncOpenAIBoundedCapture) Write(p []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if int64(w.buf.Len())+int64(len(p)) > w.limit {
		w.overflow = true
		return len(p), nil
	}
	if !w.overflow {
		_, _ = w.buf.Write(p)
	}
	return len(p), nil
}
func (w *syncOpenAIBoundedCapture) snapshot() ([]byte, bool) {
	w.mu.Lock()
	defer w.mu.Unlock()
	return bytes.Clone(w.buf.Bytes()), !w.overflow
}

type syncOpenAICaptureBody struct {
	io.Reader
	io.Closer
}
