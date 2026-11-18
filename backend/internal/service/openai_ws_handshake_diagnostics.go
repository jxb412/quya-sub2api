package service

import (
	"bytes"
	"encoding/json"
	"net/http"
	"strings"

	"github.com/Wei-Shaw/sub2api/internal/pkg/logger"
)

// Diagnostics contain only fixed categories, booleans and counts.
// Never log request/response bodies, credentials, arbitrary upstream error
// strings, full response headers, session identifiers or fingerprint seeds.
type openAIWSHandshakeDiagnostics struct {
	ServerCloudflare bool
	ContentKind      string
	BodyKind         string
	BodyBytes        int
}

func classifyOpenAIWSHandshakeResponse(headers http.Header, body []byte) openAIWSHandshakeDiagnostics {
	d := openAIWSHandshakeDiagnostics{ContentKind: "other", BodyKind: "other", BodyBytes: len(body)}
	d.ServerCloudflare = strings.EqualFold(strings.TrimSpace(headers.Get("Server")), "cloudflare")
	switch strings.ToLower(strings.TrimSpace(strings.SplitN(headers.Get("Content-Type"), ";", 2)[0])) {
	case "":
		d.ContentKind = "none"
	case "application/json":
		d.ContentKind = "json"
	case "text/html":
		d.ContentKind = "html"
	case "text/plain":
		d.ContentKind = "text"
	}
	trimmed := bytes.TrimSpace(body)
	switch {
	case len(trimmed) == 0:
		d.BodyKind = "empty"
	case json.Valid(trimmed):
		d.BodyKind = "json"
	case bytes.HasPrefix(bytes.ToLower(trimmed), []byte("<!doctype html")), bytes.HasPrefix(bytes.ToLower(trimmed), []byte("<html")):
		d.BodyKind = "html"
	}
	return d
}

func logOpenAIWSPoolHandshakeFailure(req openAIWSAcquireRequest, status int, responseHeaders http.Header, body []byte, err error) {
	if req.StrictIdentity == "" || req.Account == nil {
		return
	}
	d := classifyOpenAIWSHandshakeResponse(responseHeaders, body)
	logger.LegacyPrintf("service.openai_gateway", "[OpenAI ctx_pool] ws_handshake_failed account=%d status=%d dial_class=%s server_cloudflare=%t content_kind=%s body_kind=%s body_bytes=%d",
		req.Account.ID, status, classifyOpenAIWSDialError(err), d.ServerCloudflare, d.ContentKind, d.BodyKind, d.BodyBytes)
}
