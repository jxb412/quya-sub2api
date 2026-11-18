package service

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"

	"github.com/tidwall/gjson"
	"github.com/tidwall/sjson"
)

// 2026-11-19：HTTP 请求彼此独立，恢复不能把增量 input 猜作完整历史。
// 只接受同一授权/作用域内已完成父响应的有界缓存；转发身份保留，turn-state
// 是可更新的续链状态，不阻止同一授权下使用完整历史重新建立连接。
type openAIHTTPWSReplayPlan struct {
	identity           string
	input              []json.RawMessage
	inputExists        bool
	complete           bool
	parentFound        bool
	parent             []json.RawMessage
	previousResponseID string
	originalInput      []json.RawMessage
}

type openAIHTTPWSRecoveredRequest struct {
	identity           string
	body               []byte
	parent             []json.RawMessage
	parentFound        bool
	previousResponseID string
	originalInput      []json.RawMessage
}

func openAIHTTPWSPreparedHandshakeHeaders(source http.Header) http.Header {
	headers := openAIHTTPWSHandshakeHeaders(source)
	stripOpenAILegacyResponsesBeta(headers)
	if beta := strings.TrimSpace(strings.Join(headers.Values("OpenAI-Beta"), ", ")); !strings.Contains(beta, "responses_websockets=") {
		if beta != "" {
			beta += ", "
		}
		headers.Set("OpenAI-Beta", beta+openAIWSBetaV2Value)
	}
	return headers
}

func openAIHTTPWSHistoryIdentity(req *http.Request, proxyURL string, account *Account, attempt *openAIHTTPWSAttempt) (string, error) {
	if req == nil || req.URL == nil || account == nil || attempt == nil {
		return "", errors.New("invalid HTTP Responses WebSocket recovery identity")
	}
	wsURL := *req.URL
	switch wsURL.Scheme {
	case "https":
		wsURL.Scheme = "wss"
	case "http":
		wsURL.Scheme = "ws"
	default:
		return "", errors.New("unsupported HTTP Responses WebSocket URL scheme")
	}
	headers := openAIHTTPWSPreparedHandshakeHeaders(req.Header)
	headers.Del(openAIWSTurnStateHeader)
	return openAIHTTPWSIdentity(wsURL.String(), proxyURL, req.Host, headers, fmt.Sprintf("%s/account:%d", attempt.scope, account.ID)), nil
}

func (s *OpenAIGatewayService) prepareOpenAIHTTPWSReplayPlan(identity string, body []byte, accounts ...*Account) *openAIHTTPWSReplayPlan {
	previous := strings.TrimSpace(gjson.GetBytes(body, "previous_response_id").String())
	var parent []json.RawMessage
	var parentFound bool
	if previous != "" {
		parent, parentFound = s.getOpenAIHTTPWSReplayStore().Get(identity, previous)
	}
	replayBody, normalizeErr := normalizeOpenAIHTTPWSReplayBody(body)
	originalInput, _, inputErr := openAIWSExtractNormalizedInputSequence(replayBody)
	var alignErr error
	if len(accounts) > 0 && accounts[0] != nil && accounts[0].IsOpenAIOAuthLike() {
		parent, alignErr = alignOpenAIHTTPWSReplayCodexCallIDs(parent, originalInput)
	}
	input, exists, err := buildOpenAIWSReplayInputSequence(parent, parentFound, replayBody, previous != "")
	return &openAIHTTPWSReplayPlan{identity: identity, input: input, inputExists: exists,
		complete:    normalizeErr == nil && inputErr == nil && alignErr == nil && err == nil && exists && (previous == "" || parentFound) && openAIHTTPWSHistorySelfContained(body, input),
		parentFound: parentFound, parent: parent, previousResponseID: previous, originalInput: originalInput}
}

func alignOpenAIHTTPWSReplayCodexCallIDs(parent, current []json.RawMessage) ([]json.RawMessage, error) {
	// 2026-11-19：缓存保存上游原始 output，但下一轮 OAuth input 已经过官方
	// call_id 规范化。仅应用同一官方规则且确切匹配当前工具输出，防止恢复丢配对。
	outputs := make(map[string]struct{})
	for _, item := range current {
		if isCodexToolCallOutputItemType(gjson.GetBytes(item, "type").String()) {
			if id := strings.TrimSpace(gjson.GetBytes(item, "call_id").String()); id != "" {
				outputs[id] = struct{}{}
			}
		}
	}
	if len(outputs) == 0 {
		return parent, nil
	}
	var aligned []json.RawMessage
	for i, item := range parent {
		typ := gjson.GetBytes(item, "type").String()
		if !isCodexToolCallContextItemType(typ) {
			continue
		}
		id := strings.TrimSpace(gjson.GetBytes(item, "call_id").String())
		if id == "" {
			continue
		}
		if _, exact := outputs[id]; exact {
			continue
		}
		normalized := normalizeCodexCallIDForItemType(typ, id)
		if normalized == id {
			continue
		}
		if _, matched := outputs[normalized]; !matched {
			continue
		}
		updated, err := sjson.SetBytes(item, "call_id", normalized)
		if err != nil {
			return parent, err
		}
		if aligned == nil {
			aligned = append([]json.RawMessage(nil), parent...)
		}
		aligned[i] = updated
	}
	if aligned == nil {
		return parent, nil
	}
	return aligned, nil
}

func normalizeOpenAIHTTPWSReplayBody(body []byte) ([]byte, error) {
	// 2026-11-19：HTTP 接受 input 字符串，但 Responses input 数组不能含裸字符串。
	// 只为恢复记录转换等价 user 消息，不改变首次正常发送的原始 HTTP/WS 请求。
	input := gjson.GetBytes(body, "input")
	if input.Type != gjson.String {
		return body, nil
	}
	items, err := json.Marshal([]map[string]string{{"type": "message", "role": "user", "content": input.String()}})
	if err != nil {
		return nil, err
	}
	return sjson.SetRawBytes(body, "input", items)
}

func openAIHTTPWSHistorySelfContained(body []byte, input []json.RawMessage) bool {
	// 2026-11-19：远端 conversation/item_reference 隐含的历史并不在当前 body，
	// 保留正常转发，但不能把缺少正文的远端引用登记为本地完整恢复证据。
	conversation := gjson.GetBytes(body, "conversation")
	if conversation.Exists() && conversation.Type != gjson.Null {
		return false
	}
	for _, item := range input {
		if gjson.GetBytes(item, "type").String() == "item_reference" {
			return false
		}
	}
	return true
}

func prepareOpenAIHTTPWSPayload(body []byte, headers http.Header) (map[string]json.RawMessage, error) {
	var payload map[string]json.RawMessage
	if err := json.Unmarshal(body, &payload); err != nil || payload == nil {
		return nil, errors.New("HTTP Responses WebSocket payload must be a JSON object")
	}
	if background, present := payload["background"]; present && string(background) != "false" && string(background) != "null" {
		return nil, errors.New("background Responses are not supported by ctx_pool transport")
	}
	if raw, present := payload["type"]; present && string(raw) != `"response.create"` {
		return nil, errors.New("HTTP Responses WebSocket payload has an unsupported event type")
	}
	delete(payload, "stream")
	delete(payload, "background")
	payload["type"] = json.RawMessage(`"response.create"`)
	if err := setOpenAIHTTPWSTurnMetadata(payload, headers.Get(openAIWSTurnMetadataHeader)); err != nil {
		return nil, err
	}
	return payload, nil
}

// 2026-11-19：恢复发生在发送 response.create 之前；只重建 input/previous，
// HTTP stream、模型、工具等原字段保留，之后 WS 重试和 HTTP 兜底使用同一完整输入。
func (p *openAIHTTPWSReplayPlan) recoverBeforeDispatch(body []byte, headers http.Header, attempt *openAIHTTPWSAttempt) ([]byte, map[string]json.RawMessage, error) {
	if attempt.sent.Load() {
		return nil, nil, errors.New("ctx_pool generation already dispatched; refusing to replay")
	}
	if !p.complete {
		return nil, nil, newOpenAIHTTPWSContinuationError("This continuation cannot be restored with complete input history. Resend with full input and omit previous_response_id.")
	}
	rebuilt, safe, err := buildOpenAIWSCurrentTurnRetryPayload(body, p.input, p.inputExists, "")
	if err != nil {
		return nil, nil, fmt.Errorf("rebuild HTTP Responses WebSocket history: %w", err)
	}
	if !safe {
		return nil, nil, newOpenAIHTTPWSContinuationError("This continuation is missing required tool call history. Resend with full input and omit previous_response_id.")
	}
	payload, err := prepareOpenAIHTTPWSPayload(rebuilt, headers)
	if err != nil {
		return nil, nil, err
	}
	attempt.recovery.Store(&openAIHTTPWSRecoveredRequest{identity: p.identity, body: rebuilt,
		parent: p.parent, parentFound: p.parentFound, previousResponseID: p.previousResponseID, originalInput: p.originalInput})
	return rebuilt, payload, nil
}

func applyOpenAIHTTPWSRecoveredBody(body []byte, identity string, attempt *openAIHTTPWSAttempt) ([]byte, error) {
	recovery := attempt.recovery.Load()
	if recovery == nil {
		return body, nil
	}
	if recovery.identity != identity {
		return nil, newOpenAIHTTPWSContinuationError("This continuation cannot be restored under a different authorization or session.")
	}
	// 2026-11-19：HTTP 兼容重试可能清理 input 内字段。使用已证明完整的父快照
	// 加“当前”增量重新构建，不能用第一次恢复体覆盖这些清理或重复追加历史。
	previous := strings.TrimSpace(gjson.GetBytes(body, "previous_response_id").String())
	if previous != "" && previous != recovery.previousResponseID {
		return nil, newOpenAIHTTPWSContinuationError("This retry refers to a different previous response and cannot reuse the recovered history.")
	}
	replayBody, err := normalizeOpenAIHTTPWSReplayBody(body)
	if err != nil {
		return nil, err
	}
	currentInput, currentExists, err := openAIWSExtractNormalizedInputSequence(replayBody)
	if err != nil {
		return nil, err
	}
	if previous == "" {
		fullInput, _, err := openAIWSExtractNormalizedInputSequence(recovery.body)
		if err != nil {
			return nil, err
		}
		if len(currentInput) == len(fullInput) && openAIWSRawItemsHasPrefix(currentInput, fullInput) {
			return body, nil
		}
		if len(currentInput) != len(recovery.originalInput) || !openAIWSRawItemsHasPrefix(currentInput, recovery.originalInput) {
			return nil, newOpenAIHTTPWSContinuationError("This retry changed its input without a verifiable continuation. Resend with complete input history.")
		}
	}
	fullInput, exists := buildOpenAIWSReplayInputSequenceFromItems(recovery.parent, recovery.parentFound, currentInput, currentExists, recovery.previousResponseID != "")
	if !openAIHTTPWSHistorySelfContained(body, fullInput) {
		return nil, newOpenAIHTTPWSContinuationError("This retry depends on remote history that is not available for local recovery.")
	}
	rebuilt, safe, err := buildOpenAIWSCurrentTurnRetryPayload(body, fullInput, exists, "")
	if err != nil {
		return nil, err
	}
	if !safe {
		return nil, newOpenAIHTTPWSContinuationError("This retry is missing required tool call history. Resend with complete input history.")
	}
	return rebuilt, nil
}

func (s *OpenAIGatewayService) prepareOpenAIHTTPWSFallback(req *http.Request, proxyURL string, account *Account, attempt *openAIHTTPWSAttempt) error {
	if attempt.sent.Load() {
		return errors.New("ctx_pool generation already dispatched; refusing to replay")
	}
	if req.GetBody == nil {
		return errors.New("HTTP Responses WebSocket request body is not replayable")
	}
	identity, err := openAIHTTPWSHistoryIdentity(req, proxyURL, account, attempt)
	if err != nil {
		return err
	}
	reader, err := req.GetBody()
	if err != nil {
		return err
	}
	body, err := io.ReadAll(reader)
	_ = reader.Close()
	if err != nil {
		return err
	}
	body, err = applyOpenAIHTTPWSRecoveredBody(body, identity, attempt)
	if err != nil {
		return err
	}
	if strings.TrimSpace(gjson.GetBytes(body, "previous_response_id").String()) == "" {
		return nil
	}
	plan := s.prepareOpenAIHTTPWSReplayPlan(identity, body, account)
	if !plan.parentFound && !attempt.continuationBound.Load() {
		// 未知 previous 可能来自上游持久存储；保持原协议语义，由上游解析。
		return nil
	}
	_, _, err = plan.recoverBeforeDispatch(body, openAIHTTPWSPreparedHandshakeHeaders(req.Header), attempt)
	return err
}

func cloneOpenAIHTTPWSFallbackRequest(req *http.Request, proxyURL string, account *Account, attempt *openAIHTTPWSAttempt) (*http.Request, error) {
	if attempt.recovery.Load() == nil {
		return req, nil
	}
	if attempt.sent.Load() {
		return nil, errors.New("ctx_pool generation already dispatched; refusing to replay")
	}
	if req.GetBody == nil {
		return nil, errors.New("HTTP Responses WebSocket request body is not replayable")
	}
	identity, err := openAIHTTPWSHistoryIdentity(req, proxyURL, account, attempt)
	if err != nil {
		return nil, err
	}
	reader, err := req.GetBody()
	if err != nil {
		return nil, err
	}
	body, err := io.ReadAll(reader)
	_ = reader.Close()
	if err != nil {
		return nil, err
	}
	body, err = applyOpenAIHTTPWSRecoveredBody(body, identity, attempt)
	if err != nil {
		return nil, err
	}
	cloned := req.Clone(req.Context())
	cloned.Body = io.NopCloser(bytes.NewReader(body))
	cloned.GetBody = func() (io.ReadCloser, error) { return io.NopCloser(bytes.NewReader(body)), nil }
	cloned.ContentLength = int64(len(body))
	cloned.Header.Del("Content-Length")
	cloned.TransferEncoding = nil
	return cloned, nil
}
