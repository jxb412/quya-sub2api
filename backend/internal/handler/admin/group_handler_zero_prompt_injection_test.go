package admin

import (
	"encoding/json"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestGroupRequestsZeroPromptInjectionTriState(t *testing.T) {
	var create CreateGroupRequest
	require.NoError(t, json.Unmarshal([]byte(`{"name":"zero-prompt","openai_zero_prompt_injection":true}`), &create))
	require.True(t, create.OpenAIZeroPromptInjection)

	var disable UpdateGroupRequest
	require.NoError(t, json.Unmarshal([]byte(`{"openai_zero_prompt_injection":false}`), &disable))
	require.NotNil(t, disable.OpenAIZeroPromptInjection)
	require.False(t, *disable.OpenAIZeroPromptInjection)

	var omitted UpdateGroupRequest
	require.NoError(t, json.Unmarshal([]byte(`{}`), &omitted))
	require.Nil(t, omitted.OpenAIZeroPromptInjection)
}
