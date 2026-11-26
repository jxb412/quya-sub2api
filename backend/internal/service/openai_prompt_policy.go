package service

import (
	"github.com/Wei-Shaw/sub2api/internal/pkg/ctxkey"
	"github.com/gin-gonic/gin"
)

const openAIZeroPromptInjectionContextKey = "quya_openai_zero_prompt_injection"

// 2026-11-26: snapshot the authenticated group's prompt policy before forwarding.
// Account retries and HTTP/WS bridges must not restore gateway-authored guidance.
// This controls our additions, never text already supplied by a client/provider.
func openAIZeroPromptInjection(c *gin.Context) bool {
	if c == nil {
		return false
	}
	if value, exists := c.Get(openAIZeroPromptInjectionContextKey); exists {
		enabled, _ := value.(bool)
		return enabled
	}
	group := apiKeyGroup(getAPIKeyFromContext(c))
	if group == nil && c.Request != nil {
		group, _ = c.Request.Context().Value(ctxkey.Group).(*Group)
	}
	enabled := group != nil && group.Platform == PlatformOpenAI && group.OpenAIZeroPromptInjection
	c.Set(openAIZeroPromptInjectionContextKey, enabled)
	return enabled
}
