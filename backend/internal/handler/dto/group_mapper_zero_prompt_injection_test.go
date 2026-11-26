package dto

import (
	"encoding/json"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/stretchr/testify/require"
)

func TestGroupMapperZeroPromptInjectionAdminOnly(t *testing.T) {
	group := &service.Group{ID: 7, Platform: service.PlatformOpenAI, OpenAIZeroPromptInjection: true}
	userJSON, err := json.Marshal(GroupFromService(group))
	require.NoError(t, err)
	require.NotContains(t, string(userJSON), "openai_zero_prompt_injection")
	adminJSON, err := json.Marshal(GroupFromServiceAdmin(group))
	require.NoError(t, err)
	require.Contains(t, string(adminJSON), `"openai_zero_prompt_injection":true`)
}
