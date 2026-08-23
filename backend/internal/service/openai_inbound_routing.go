package service

import (
	"context"
	"net/http"
	"strings"

	"github.com/gin-gonic/gin"
)

const OpenAIInboundProtocolRestrictionReason GatewayFailureReason = "openai_inbound_protocol_restriction"

// OpenAIInboundProtocol identifies the public protocol used by the client.
// It is intentionally separate from OpenAIEndpointCapability, which describes
// what an upstream account can do after the gateway has selected it.
type OpenAIInboundProtocol string

const (
	OpenAIInboundProtocolResponses       OpenAIInboundProtocol = "responses"
	OpenAIInboundProtocolChatCompletions OpenAIInboundProtocol = "chat_completions"
	OpenAIInboundProtocolMessages        OpenAIInboundProtocol = "messages"
)

// OpenAIInboundRequestFilter is attached to a request context by the gateway
// handler. The scheduler invokes it before scoring candidates, so an account
// that is incompatible with the client never enters the candidate pool.
type OpenAIInboundRequestFilter func(*Account) bool

type openAIInboundRequestContextKey struct{}

type openAIInboundRequestPolicy struct {
	Protocol OpenAIInboundProtocol
	Filter   OpenAIInboundRequestFilter
}

// WithOpenAIInboundRequestPolicy carries the public protocol and any
// account-level client policy into all scheduler paths, including sticky,
// legacy, and advanced load-aware selection.
func WithOpenAIInboundRequestPolicy(ctx context.Context, protocol OpenAIInboundProtocol, filter OpenAIInboundRequestFilter) context.Context {
	if ctx == nil {
		ctx = context.Background()
	}
	return context.WithValue(ctx, openAIInboundRequestContextKey{}, openAIInboundRequestPolicy{
		Protocol: protocol,
		Filter:   filter,
	})
}

func openAIInboundRequestPolicyFromContext(ctx context.Context) (openAIInboundRequestPolicy, bool) {
	if ctx == nil {
		return openAIInboundRequestPolicy{}, false
	}
	policy, ok := ctx.Value(openAIInboundRequestContextKey{}).(openAIInboundRequestPolicy)
	return policy, ok
}

func openAIInboundAccountAllowed(ctx context.Context, account *Account) bool {
	policy, ok := openAIInboundRequestPolicyFromContext(ctx)
	if !ok || account == nil {
		return true
	}
	if !account.SupportsOpenAIInboundProtocol(policy.Protocol) {
		return false
	}
	if policy.Filter != nil && !policy.Filter(account) {
		return false
	}
	return true
}

// SupportsOpenAIInboundProtocol applies account-level public protocol routing.
// Existing accounts have no inbound capability metadata and remain eligible
// for all protocols. This setting is independent from codex_cli_only: the
// latter checks client identity, while this method checks the public endpoint.
func (a *Account) SupportsOpenAIInboundProtocol(protocol OpenAIInboundProtocol) bool {
	if a == nil || strings.TrimSpace(string(protocol)) == "" {
		return true
	}
	if a.IsOpenAIResponsesOnly() && protocol != OpenAIInboundProtocolResponses {
		return false
	}

	raw, exists := a.Extra["openai_inbound_capabilities"]
	if !exists || raw == nil {
		return true
	}

	allowed := make(map[string]struct{})
	add := func(value any) {
		if value == nil {
			return
		}
		if text, ok := value.(string); ok {
			text = strings.TrimSpace(strings.ToLower(text))
			if text != "" {
				allowed[text] = struct{}{}
			}
		}
	}
	switch values := raw.(type) {
	case []string:
		for _, value := range values {
			add(value)
		}
	case []any:
		for _, value := range values {
			add(value)
		}
	case map[string]any:
		for key, value := range values {
			if enabled, ok := value.(bool); ok && enabled {
				add(key)
			}
		}
	default:
		// Malformed/legacy metadata is fail-open for compatibility. The
		// forward-time policy check remains the final guard.
		return true
	}
	if len(allowed) == 0 {
		return true
	}
	_, ok := allowed[strings.ToLower(strings.TrimSpace(string(protocol)))]
	return ok
}

// IsOpenAIResponsesOnly reports the independent account switch that reserves
// the account for the public /v1/responses family. It does not inspect client
// identity; combine it with codex_cli_only only when both restrictions are
// intentionally required.
func (a *Account) IsOpenAIResponsesOnly() bool {
	if a == nil || !a.IsOpenAI() || a.Extra == nil {
		return false
	}
	enabled, ok := a.Extra["openai_responses_only"].(bool)
	return ok && enabled
}

// IsOpenAICodexClientAllowedForAccount evaluates the existing account-level
// codex_cli_only policy for scheduler pre-filtering. Unrestricted accounts are
// always eligible; restricted accounts must pass the same detector used by the
// forwarding layer.
func (s *OpenAIGatewayService) IsOpenAICodexClientAllowedForAccount(c *gin.Context, account *Account, body []byte) bool {
	if account == nil || !account.IsCodexCLIOnlyEnabled() {
		return true
	}
	if s == nil {
		return false
	}
	if c == nil {
		return false
	}
	result := s.detectCodexClientRestriction(c, account, body)
	return result.Matched
}

func openAIInboundProtocolFailover(c *gin.Context, account *Account) *UpstreamFailoverError {
	if c == nil || c.Request == nil || account == nil {
		return nil
	}
	if openAIInboundAccountAllowed(c.Request.Context(), account) {
		return nil
	}
	return &UpstreamFailoverError{
		StatusCode:             http.StatusForbidden,
		Scope:                  GatewayFailureScopeRequest,
		Reason:                 OpenAIInboundProtocolRestrictionReason,
		RequestScopedTransient: true,
		NextAccountAction:      NextAccountRetry,
		ClientStatusCode:       http.StatusForbidden,
		ClientMessage:          "No account in this group accepts the current client protocol",
	}
}
