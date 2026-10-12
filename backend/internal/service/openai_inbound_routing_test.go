package service

import (
	"context"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestAccountSupportsOpenAIInboundProtocol(t *testing.T) {
	restricted := &Account{
		Platform: PlatformOpenAI,
		Type:     AccountTypeOAuth,
		Extra:    map[string]any{"codex_cli_only": true},
	}
	for _, protocol := range []OpenAIInboundProtocol{
		OpenAIInboundProtocolChatCompletions,
		OpenAIInboundProtocolMessages,
	} {
		require.True(t, restricted.SupportsOpenAIInboundProtocol(protocol))
	}
	require.True(t, restricted.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolResponses))

	responsesOnly := &Account{
		Platform: PlatformOpenAI,
		Type:     AccountTypeOAuth,
		Extra:    map[string]any{"openai_responses_only": true},
	}
	require.True(t, responsesOnly.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolResponses))
	require.False(t, responsesOnly.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolChatCompletions))
	require.False(t, responsesOnly.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolMessages))

	combined := &Account{
		Platform: PlatformOpenAI,
		Type:     AccountTypeOAuth,
		Extra: map[string]any{
			"codex_cli_only":        true,
			"openai_responses_only": true,
		},
	}
	require.True(t, combined.IsCodexCLIOnlyEnabled())
	require.True(t, combined.IsOpenAIResponsesOnly())
	require.False(t, combined.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolChatCompletions))

	legacy := &Account{Platform: PlatformOpenAI, Type: AccountTypeOAuth}
	require.True(t, legacy.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolResponses))
	require.True(t, legacy.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolChatCompletions))
	require.True(t, legacy.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolMessages))
}

// API Key 账号与 OAuth 账号走同一套访问限制：入口协议过滤（responses only）
// 与客户端身份过滤（codex cli only）都必须对前者同样生效。
func TestAccountInboundPolicyForOpenAIAPIKey(t *testing.T) {
	responsesOnly := &Account{
		Platform: PlatformOpenAI,
		Type:     AccountTypeAPIKey,
		Extra:    map[string]any{"openai_responses_only": true},
	}
	require.True(t, responsesOnly.IsOpenAIResponsesOnly())
	require.True(t, responsesOnly.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolResponses))
	require.False(t, responsesOnly.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolChatCompletions))
	require.False(t, responsesOnly.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolMessages))

	codexOnly := &Account{
		Platform: PlatformOpenAI,
		Type:     AccountTypeAPIKey,
		Extra:    map[string]any{"codex_cli_only": true},
	}
	require.True(t, codexOnly.IsCodexCLIOnlyEnabled())
	// 只限制客户端，不限入口协议，与 OAuth 账号一致。
	require.True(t, codexOnly.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolResponses))
	require.True(t, codexOnly.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolChatCompletions))
	require.True(t, codexOnly.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolMessages))

	combined := &Account{
		Platform: PlatformOpenAI,
		Type:     AccountTypeAPIKey,
		Extra: map[string]any{
			"codex_cli_only":        true,
			"openai_responses_only": true,
		},
	}
	require.True(t, combined.IsCodexCLIOnlyEnabled())
	require.False(t, combined.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolChatCompletions))
}

func TestAccountSupportsExplicitOpenAIInboundCapabilities(t *testing.T) {
	account := &Account{
		Platform: PlatformOpenAI,
		Type:     AccountTypeOAuth,
		Extra: map[string]any{
			"openai_inbound_capabilities": []any{"responses"},
		},
	}
	require.True(t, account.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolResponses))
	require.False(t, account.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolChatCompletions))
	require.False(t, account.SupportsOpenAIInboundProtocol(OpenAIInboundProtocolMessages))
}

func TestOpenAIInboundRequestPolicyFiltersBeforeSchedulerEligibility(t *testing.T) {
	restricted := &Account{Platform: PlatformOpenAI, Type: AccountTypeOAuth}
	allowed := &Account{Platform: PlatformOpenAI, Type: AccountTypeOAuth}
	ctx := WithOpenAIInboundRequestPolicy(
		context.Background(),
		OpenAIInboundProtocolResponses,
		func(account *Account) bool { return account == allowed },
	)
	require.False(t, openAIInboundAccountAllowed(ctx, restricted))
	require.True(t, openAIInboundAccountAllowed(ctx, allowed))
}
