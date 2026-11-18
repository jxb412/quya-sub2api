package service

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/tidwall/gjson"
)

// Connection establishment failures authorize bounded retries before HTTP
// fallback. The separate Codex capacity error permits immediate fallback;
// payload validation and any attempted generation write never permit it.
type openAIHTTPWSConnectError struct{ err error }

func (e *openAIHTTPWSConnectError) Error() string {
	return "acquire HTTP Responses WebSocket: " + e.err.Error()
}
func (e *openAIHTTPWSConnectError) Unwrap() error { return e.err }

// doOpenAIHTTPWSPool changes only transport: the caller has already applied the
// endpoint-specific transforms, account identity, model policy and credentials.
// A submitted response.create is never replayed here, even if its write/read
// fails before any downstream output. The caller owns the dispatch guard.
func (s *OpenAIGatewayService) doOpenAIHTTPWSPool(req *http.Request, proxyURL string, account *Account) (*http.Response, error) {
	if req == nil || req.URL == nil || account == nil || req.Method != http.MethodPost {
		return nil, errors.New("invalid HTTP Responses WebSocket request")
	}
	attempt := openAIHTTPWSAttemptFromContext(req.Context())
	if attempt == nil || strings.TrimSpace(attempt.scope) == "" {
		return nil, errors.New("HTTP Responses WebSocket request has no authenticated scope")
	}
	if attempt.sent.Load() {
		return nil, errors.New("HTTP Responses WebSocket request already dispatched")
	}
	attempt.used.Store(true)
	if !strings.HasSuffix(strings.TrimRight(req.URL.Path, "/"), "/responses") {
		return nil, errors.New("HTTP Responses WebSocket transport requires the Responses endpoint")
	}
	if req.GetBody == nil {
		return nil, errors.New("HTTP Responses WebSocket request body is not replayable")
	}
	reader, err := req.GetBody()
	if err != nil {
		return nil, fmt.Errorf("read HTTP Responses WebSocket payload: %w", err)
	}
	// This is the prepared HTTP body, already admitted by the HTTP ingress
	// size limits. The upstream event read limit is not a request write limit:
	// applying it here rejects otherwise valid long conversations above 16 MiB.
	body, err := io.ReadAll(reader)
	_ = reader.Close()
	if err != nil {
		return nil, fmt.Errorf("read HTTP Responses WebSocket payload: %w", err)
	}
	wsURL := *req.URL
	switch wsURL.Scheme {
	case "https":
		wsURL.Scheme = "wss"
	case "http":
		wsURL.Scheme = "ws"
	default:
		return nil, errors.New("unsupported HTTP Responses WebSocket URL scheme")
	}
	headers := openAIHTTPWSPreparedHandshakeHeaders(req.Header)
	historyIdentity, err := openAIHTTPWSHistoryIdentity(req, proxyURL, account, attempt)
	if err != nil {
		return nil, err
	}
	body, err = applyOpenAIHTTPWSRecoveredBody(body, historyIdentity, attempt)
	if err != nil {
		return nil, err
	}
	payload, err := prepareOpenAIHTTPWSPayload(body, headers)
	if err != nil {
		return nil, err
	}
	identity := openAIHTTPWSIdentity(wsURL.String(), proxyURL, req.Host, headers, attempt.scope)
	acquire := openAIWSAcquireRequest{Account: account, WSURL: wsURL.String(), Headers: headers, ProxyURL: proxyURL, StrictIdentity: identity}
	acquire.ForceNewConn = attempt.recovery.Load() != nil
	// 2026-11-19：与官方原生链路一致，完整性来自已成功父轮的输入和全部输出，
	// 不以“input 非空”推断。原连接可用时优先保留 previous_response_id。
	replayPlan := s.prepareOpenAIHTTPWSReplayPlan(historyIdentity, body, account)
	stateStore := s.getOpenAIWSStateStore()
	previousResponseID := strings.TrimSpace(gjson.GetBytes(body, "previous_response_id").String())
	if stateStore != nil && previousResponseID != "" {
		if connID, ok := stateStore.GetResponseConn(previousResponseID); ok {
			acquire.PreferredConnID = connID
			acquire.ForcePreferredConn = true
			attempt.continuationBound.Store(true)
		}
	}
	recoverBeforeDispatch := func() error {
		var recoveryErr error
		body, payload, recoveryErr = replayPlan.recoverBeforeDispatch(body, headers, attempt)
		if recoveryErr != nil {
			return recoveryErr
		}
		previousResponseID = ""
		acquire.PreferredConnID = ""
		acquire.ForcePreferredConn = false
		acquire.ForceNewConn = true
		return checkOpenAIHTTPWSCodexPayloadSize(req, account, payload)
	}
	if previousResponseID != "" && !acquire.ForcePreferredConn && replayPlan.parentFound {
		if err := recoverBeforeDispatch(); err != nil {
			return nil, err
		}
	}
	if err := checkOpenAIHTTPWSCodexPayloadSize(req, account, payload); err != nil {
		var capacityErr *openAIHTTPWSCodexCapacityError
		if previousResponseID != "" && (acquire.ForcePreferredConn || replayPlan.parentFound) && errors.As(err, &capacityErr) {
			if err := recoverBeforeDispatch(); err != nil {
				return nil, err
			}
		} else {
			return nil, err
		}
	}
	// Only connection acquisition is bounded by five seconds. The successful
	// generation uses the original request context and normal stream timeout.
	acquireCtx, cancelAcquire := context.WithTimeout(req.Context(), 5*time.Second)
	defer cancelAcquire()
	lease, err := s.getOpenAIWSConnPool().Acquire(acquireCtx, acquire)
	if err != nil && acquire.ForcePreferredConn && errors.Is(err, errOpenAIWSPreferredConnUnavailable) {
		if err := recoverBeforeDispatch(); err != nil {
			return nil, err
		}
		lease, err = s.getOpenAIWSConnPool().Acquire(acquireCtx, acquire)
	}
	if err == nil && account.IsOpenAIOAuthLike() && lease.initializedWithoutWebSearch() && openAIWSWebSearchDeclared(body) {
		lease.Release()
		if previousResponseID != "" {
			if err := recoverBeforeDispatch(); err != nil {
				return nil, err
			}
		}
		// No generation has been written. Preserve the complete prepared
		// request and use a fresh socket whose first create includes search.
		acquire.ForceNewConn = true
		lease, err = s.getOpenAIWSConnPool().Acquire(acquireCtx, acquire)
	}
	if err != nil {
		var dialErr *openAIWSDialError
		if errors.As(err, &dialErr) && dialErr.StatusCode >= 400 && dialErr.StatusCode <= 599 {
			return &http.Response{StatusCode: dialErr.StatusCode, Status: fmt.Sprintf("%d %s", dialErr.StatusCode, http.StatusText(dialErr.StatusCode)), Header: cloneHeader(dialErr.ResponseHeaders), Body: io.NopCloser(bytes.NewReader(dialErr.ResponseBody)), ContentLength: int64(len(dialErr.ResponseBody)), Request: req}, nil
		}
		return nil, &openAIHTTPWSConnectError{err: err}
	}
	// This flag is set before Write, not after it: a failed write can still have
	// delivered the request, so its outcome must never authorize a second send.
	if !attempt.sent.CompareAndSwap(false, true) {
		lease.Release()
		return nil, errors.New("HTTP Responses WebSocket request already dispatched")
	}
	attempt.reused.Store(lease.Reused())
	if err := lease.WriteJSONWithContextTimeout(req.Context(), payload, s.openAIWSWriteTimeout()); err != nil {
		lease.MarkBroken()
		lease.Release()
		return nil, fmt.Errorf("write HTTP Responses WebSocket request: %w", err)
	}
	responseHeaders := make(http.Header)
	if !lease.Reused() {
		// Handshake quota/request headers describe the opening attempt only.
		// Reusing them on later turns would overwrite fresh quota with old data.
		responseHeaders = openAIHTTPWSHandshakeHeaders(lease.HandshakeHeaders())
	}
	responseHeaders.Set("Content-Type", "text/event-stream")
	responseHeaders.Set("X-Sub2api-Upstream-Transport", "websocket")
	bodyCtx, cancelBody := context.WithCancel(req.Context())
	stream := &openAIHTTPWSSSEBody{ctx: bodyCtx, cancel: cancelBody, lease: lease, readTimeout: s.openAIWSReadTimeout()}
	stream.onEvent = func(responseID, eventType string) {
		if responseID != "" {
			attempt.responseID.Store(responseID)
		}
		if isOpenAIWSTerminalEvent(eventType) || eventType == "error" {
			attempt.terminalEvent.Store(eventType)
		}
	}
	stream.onComplete = func(responseID string) {
		// 2026-11-19：只有已校验成功终态的完整 output 数组才能补全历史；
		// 缺少 output、未知父链或中途断流仍按原逻辑响应，但不能伪造可恢复缓存。
		if responseID != "" && replayPlan.complete && stream.completedOutputExists {
			s.getOpenAIHTTPWSReplayStore().Put(historyIdentity, responseID,
				combineOpenAIWSReplayItems(replayPlan.input, stream.completedOutput), s.openAIWSResponseStickyTTL())
		}
		if stateStore != nil && responseID != "" {
			stateStore.BindResponseConn(responseID, lease.ConnID(), s.openAIWSResponseStickyTTL())
			bindCtx, cancel := context.WithTimeout(context.WithoutCancel(req.Context()), 3*time.Second)
			defer cancel()
			logOpenAIWSBindResponseAccountWarn(attempt.groupID, account.ID, responseID,
				stateStore.BindResponseAccount(bindCtx, attempt.groupID, responseID, account.ID, s.openAIWSResponseStickyTTL()))
		}
	}
	return &http.Response{StatusCode: http.StatusOK, Status: "200 OK", Header: responseHeaders, Body: stream, ContentLength: -1, Request: req}, nil
}

func openAIHTTPWSHandshakeHeaders(source http.Header) http.Header {
	headers := cloneHeader(source)
	if headers == nil {
		headers = make(http.Header)
	}
	for _, name := range strings.Split(headers.Get("Connection"), ",") {
		headers.Del(strings.TrimSpace(name))
	}
	for name := range headers {
		lower := strings.ToLower(name)
		switch lower {
		case "connection", "upgrade", "content-length", "content-type", "accept", "accept-encoding", "transfer-encoding", "keep-alive", "proxy-connection", "te", "trailer", "sec-websocket-key", "sec-websocket-version", "sec-websocket-extensions", "sec-websocket-accept", "sec-websocket-protocol":
			delete(headers, name)
		default:
			if strings.HasPrefix(lower, "x-sub2api-") || strings.HasPrefix(lower, "x-sub2-") {
				delete(headers, name)
			}
		}
	}
	return headers
}

// Quya 2026-11-19：池连接的握手头不会逐轮更新，当前 turn metadata 随每次
// response.create 传递；保留客户其他 metadata 及大整数精度，避免复用旧轮信息。
func setOpenAIHTTPWSTurnMetadata(payload map[string]json.RawMessage, turnMetadata string) error {
	metadata := strings.TrimSpace(turnMetadata)
	if metadata == "" {
		return nil
	}
	var clientMetadata map[string]json.RawMessage
	if raw, ok := payload["client_metadata"]; ok {
		if err := json.Unmarshal(raw, &clientMetadata); err != nil {
			return errors.New("HTTP Responses WebSocket client_metadata must be an object or null")
		}
	}
	if clientMetadata == nil {
		clientMetadata = make(map[string]json.RawMessage)
	}
	encodedMetadata, err := json.Marshal(metadata)
	if err != nil {
		return errors.New("encode HTTP Responses WebSocket turn metadata")
	}
	clientMetadata[openAIWSTurnMetadataHeader] = encodedMetadata
	encodedClientMetadata, err := json.Marshal(clientMetadata)
	if err != nil {
		return errors.New("encode HTTP Responses WebSocket client metadata")
	}
	payload["client_metadata"] = encodedClientMetadata
	return nil
}

func openAIHTTPWSIdentity(wsURL, proxyURL, host string, headers http.Header, scope string) string {
	canonical := make(map[string][]string, len(headers))
	keys := make([]string, 0, len(headers))
	for key := range headers {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	for _, key := range keys {
		lower := strings.ToLower(key)
		// Request tracing changes every turn and does not identify a conversation.
		// Authentication, cookies, session and fingerprint headers remain exact.
		switch lower {
		case "x-request-id", "x-client-request-id", "request-id", "traceparent", "tracestate", "baggage", "x-b3-traceid", "x-b3-spanid", "x-b3-parentspanid", "x-b3-sampled", "x-b3-flags":
			continue
		case openAIWSTurnMetadataHeader, openAICodexRoutingHintHeader:
			// Quya 2026-11-19：这两项逐轮变化却不定义客户身份，排除后减少
			// 不必要的新握手；认证、会话、指纹与 turn-state 仍严格隔离。
			// Turn metadata travels with each response.create; routing hints are
			// advisory preferences in the native pool, not session identity.
			continue
		}
		canonical[lower] = append(canonical[lower], headers[key]...)
	}
	encoded, _ := json.Marshal([]any{wsURL, proxyURL, host, canonical, scope})
	digest := sha256.Sum256(encoded)
	return hex.EncodeToString(digest[:])
}

// Read serializes a single upstream turn to SSE without a goroutine or an
// unbounded event queue. Close cancels a blocked read before acquiring mu.
type openAIHTTPWSSSEBody struct {
	mu                    sync.Mutex
	ctx                   context.Context
	cancel                context.CancelFunc
	lease                 *openAIWSConnLease
	readTimeout           time.Duration
	pending               []byte
	documents             [][]byte
	responseID            string
	terminal              bool
	success               bool
	completedOutput       []json.RawMessage
	completedOutputExists bool
	// 2026-11-19：只记是否收到输出事件，避免空终态把已生成正文从恢复历史中抹掉。
	outputObserved bool
	closed         bool
	onEvent        func(string, string)
	onComplete     func(string)
}

func (b *openAIHTTPWSSSEBody) Read(dst []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if len(dst) == 0 {
		return 0, nil
	}
	if b.closed && b.terminal {
		return 0, io.EOF
	}
	if b.closed {
		return 0, io.ErrClosedPipe
	}
	if len(b.pending) == 0 && b.terminal {
		b.finishLocked()
		return 0, io.EOF
	}
	if len(b.pending) == 0 {
		var message []byte
		if len(b.documents) != 0 {
			message, b.documents = b.documents[0], b.documents[1:]
		} else {
			var err error
			message, err = b.lease.ReadMessageWithContextTimeout(b.ctx, b.readTimeout)
			if err != nil {
				b.finishLocked()
				return 0, fmt.Errorf("read HTTP Responses WebSocket before terminal: %w", err)
			}
			if int64(len(message)) > openAIWSMessageReadLimitBytes {
				b.finishLocked()
				return 0, errors.New("HTTP Responses WebSocket event exceeds message limit")
			}
			if documents, repaired := splitOpenAIConcatenatedJSONDocuments(message); repaired {
				message, b.documents = documents[0], documents[1:]
			}
		}
		if err := b.validateEvent(message); err != nil {
			b.success = false
			b.finishLocked()
			return 0, err
		}
		var compact bytes.Buffer
		if err := json.Compact(&compact, message); err != nil {
			b.success = false
			b.finishLocked()
			return 0, errors.New("malformed HTTP Responses WebSocket JSON event")
		}
		b.pending = append([]byte("data: "), compact.Bytes()...)
		b.pending = append(b.pending, '\n', '\n')
	}
	n := copy(dst, b.pending)
	b.pending = b.pending[n:]
	return n, nil
}

func (b *openAIHTTPWSSSEBody) validateEvent(message []byte) error {
	if !json.Valid(message) {
		return errors.New("malformed HTTP Responses WebSocket JSON event")
	}
	document := gjson.ParseBytes(message)
	if !document.IsObject() || document.Get("type").Type != gjson.String || strings.TrimSpace(document.Get("type").String()) == "" {
		return errors.New("HTTP Responses WebSocket event must be an object with a nonempty string type")
	}
	typeName, _, response := parseOpenAIWSEventEnvelope(message)
	if openAIHTTPWSHasOutputEvent(typeName) {
		b.outputObserved = true
	}
	if !strings.HasPrefix(typeName, "response.") && typeName != "error" {
		// Native Responses WS also emits ancillary events. Preserve them as SSE
		// without treating their IDs as generation identity or terminal state.
		return nil
	}
	// A top-level id may identify the event/item, never infer a response ID
	// from it. Stream events carry response_id or response.id explicitly.
	responseID := response.Get("id").String()
	if explicit := gjson.GetBytes(message, "response_id").String(); explicit != "" {
		if responseID != "" && explicit != responseID {
			return errors.New("HTTP Responses WebSocket event has conflicting response identities")
		}
		responseID = explicit
	}
	if responseID != "" {
		if b.responseID != "" && responseID != b.responseID {
			return errors.New("HTTP Responses WebSocket response identity changed during turn")
		}
		b.responseID = responseID
	}
	if !isOpenAIWSTerminalEvent(typeName) && typeName != "error" {
		if b.onEvent != nil {
			b.onEvent(b.responseID, typeName)
		}
		return nil
	}
	if len(b.documents) != 0 {
		return errors.New("HTTP Responses WebSocket terminal event has trailing documents")
	}
	terminalEvent := typeName
	if typeName != "error" {
		if responseID == "" || !response.IsObject() {
			return errors.New("HTTP Responses WebSocket terminal is missing its response")
		}
		status := response.Get("status").String()
		expected := strings.TrimPrefix(typeName, "response.")
		if expected == "done" {
			if status != "completed" && status != "failed" && status != "incomplete" && status != "cancelled" && status != "canceled" {
				return errors.New("HTTP Responses WebSocket done event has nonterminal status")
			}
			if status != "completed" {
				// The original event remains untouched for downstream clients,
				// while scheduling must distinguish a failed done from success.
				terminalEvent = "response." + status
			}
		} else if status != expected {
			return errors.New("HTTP Responses WebSocket terminal status contradicts its event type")
		}
		b.success = status == "completed"
		if b.success {
			// 2026-11-19：不累计无界 output_item 事件；终态完整数组已受单消息大小限制。
			output := response.Get("output")
			if output.IsArray() && json.Unmarshal([]byte(output.Raw), &b.completedOutput) == nil {
				b.completedOutputExists = len(b.completedOutput) != 0 || !b.outputObserved
				for _, item := range b.completedOutput {
					value := gjson.ParseBytes(item)
					if !value.IsObject() || value.Get("type").String() == "" || value.Get("type").String() == "item_reference" {
						b.completedOutputExists = false
						break
					}
				}
			}
		}
	}
	b.terminal = true
	if b.onEvent != nil {
		b.onEvent(b.responseID, terminalEvent)
	}
	return nil
}

func (b *openAIHTTPWSSSEBody) finishLocked() {
	if b.closed {
		return
	}
	b.closed = true
	b.cancel()
	if !b.terminal || !b.success {
		b.lease.MarkBroken()
	} else if b.onComplete != nil {
		b.onComplete(b.responseID)
	}
	b.lease.Release()
}

func (b *openAIHTTPWSSSEBody) Close() error {
	b.cancel()
	b.mu.Lock()
	defer b.mu.Unlock()
	b.finishLocked()
	return nil
}
