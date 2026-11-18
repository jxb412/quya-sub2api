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

// Only connection establishment failures may authorize the caller's bounded
// HTTP fallback. Payload validation and any attempted generation write may not.
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
	body, err := io.ReadAll(io.LimitReader(reader, openAIWSMessageReadLimitBytes+1))
	_ = reader.Close()
	if err != nil || int64(len(body)) > openAIWSMessageReadLimitBytes {
		return nil, errors.New("HTTP Responses WebSocket payload exceeds the message limit or cannot be read")
	}
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

	wsURL := *req.URL
	switch wsURL.Scheme {
	case "https":
		wsURL.Scheme = "wss"
	case "http":
		wsURL.Scheme = "ws"
	default:
		return nil, errors.New("unsupported HTTP Responses WebSocket URL scheme")
	}
	headers := openAIHTTPWSHandshakeHeaders(req.Header)
	if beta := strings.TrimSpace(headers.Get("OpenAI-Beta")); !strings.Contains(beta, "responses_websockets=") {
		if beta != "" {
			beta += ", "
		}
		headers.Set("OpenAI-Beta", beta+openAIWSBetaV2Value)
	}
	identity := openAIHTTPWSIdentity(wsURL.String(), proxyURL, req.Host, headers, attempt.scope)
	acquire := openAIWSAcquireRequest{Account: account, WSURL: wsURL.String(), Headers: headers, ProxyURL: proxyURL, StrictIdentity: identity}
	stateStore := s.getOpenAIWSStateStore()
	previousResponseID := gjson.GetBytes(body, "previous_response_id").String()
	if stateStore != nil && previousResponseID != "" {
		if connID, ok := stateStore.GetResponseConn(previousResponseID); ok {
			acquire.PreferredConnID = connID
			acquire.ForcePreferredConn = true
		}
	}
	// Only connection acquisition is bounded by five seconds. The successful
	// generation uses the original request context and normal stream timeout.
	acquireCtx, cancelAcquire := context.WithTimeout(req.Context(), 5*time.Second)
	lease, err := s.getOpenAIWSConnPool().Acquire(acquireCtx, acquire)
	cancelAcquire()
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
	mu          sync.Mutex
	ctx         context.Context
	cancel      context.CancelFunc
	lease       *openAIWSConnLease
	readTimeout time.Duration
	pending     []byte
	documents   [][]byte
	responseID  string
	terminal    bool
	success     bool
	closed      bool
	onEvent     func(string, string)
	onComplete  func(string)
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
