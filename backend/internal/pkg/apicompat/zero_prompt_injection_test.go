package apicompat

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestZeroPromptInjectionToolMediaKeepsCallerTextAndImages(t *testing.T) {
	input := json.RawMessage(`[
		{"type":"function_call","call_id":"call_image","name":"view_image","arguments":"{}"},
		{"type":"function_call_output","call_id":"call_image","output":[{"type":"input_text","text":"[Tool output media moved to the following user message]"},{"type":"input_image","image_url":"data:image/png;base64,AQID"}]}
	]`)
	for _, zero := range []bool{false, true} {
		request := &ResponsesRequest{Model: "customer-model", Input: input}
		chat, err := ResponsesToChatCompletionsRequestWithOptions(request, &ResponsesToChatOptions{ZeroPromptInjection: zero})
		require.NoError(t, err)
		require.Len(t, chat.Messages, 3)
		var toolText string
		require.NoError(t, json.Unmarshal(chat.Messages[1].Content, &toolText))
		expectedMarkers := 2
		if zero {
			expectedMarkers = 1 // The identical marker supplied by the caller survives.
		}
		require.Equal(t, expectedMarkers, strings.Count(toolText, toolOutputMediaMarker))
		var media []ChatContentPart
		require.NoError(t, json.Unmarshal(chat.Messages[2].Content, &media))
		if zero {
			require.Len(t, media, 1)
		} else {
			require.Len(t, media, 2)
		}
		require.Equal(t, "data:image/png;base64,AQID", media[len(media)-1].ImageURL.URL)

		var nativeInput any
		require.NoError(t, json.Unmarshal(input, &nativeInput))
		lifted, changed := LiftResponsesToolOutputMediaWithPromptPolicy(nativeInput, zero)
		require.True(t, changed)
		encoded, err := json.Marshal(lifted)
		require.NoError(t, err)
		require.Equal(t, expectedMarkers, strings.Count(string(encoded), toolOutputMediaMarker))
		require.Contains(t, string(encoded), "data:image/png;base64,AQID")
		require.Equal(t, !zero, strings.Contains(string(encoded), "Tool output media for call"))
	}
}

func TestZeroPromptInjectionToolAdaptersPreserveClientDescriptions(t *testing.T) {
	const callerDescription = "Search and load Codex tools, plugins, connectors, and MCP namespaces for the current task."
	for _, zero := range []bool{false, true} {
		tools := []ResponsesTool{{Type: "tool_search", Description: callerDescription}, {Type: "custom", Name: "exec", Description: "caller tool description"}}
		chat, err := ResponsesToChatCompletionsRequestWithOptions(&ResponsesRequest{Model: "customer-model", Tools: tools}, &ResponsesToChatOptions{ZeroPromptInjection: zero})
		require.NoError(t, err)
		require.Equal(t, callerDescription, chat.Tools[0].Function.Description)
		require.Equal(t, "caller tool description", chat.Tools[1].Function.Description)
		for _, tool := range chat.Tools {
			require.Equal(t, !zero, strings.Contains(string(tool.Function.Parameters), `"description"`))
		}
		request := map[string]any{"tools": []any{map[string]any{"type": "tool_search", "description": callerDescription}, map[string]any{"type": "custom", "name": "exec", "description": "caller tool description"}}}
		mapping, changed, err := AdaptResponsesClientToolsWithPromptPolicy(request, zero)
		require.NoError(t, err)
		require.True(t, changed)
		lowered, ok := request["tools"].([]any)
		require.True(t, ok)
		require.Len(t, lowered, 2)
		firstTool, ok := lowered[0].(map[string]any)
		require.True(t, ok)
		require.Equal(t, callerDescription, firstTool["description"])
		for _, tool := range lowered {
			toolMap, ok := tool.(map[string]any)
			require.True(t, ok)
			encoded, err := json.Marshal(toolMap["parameters"])
			require.NoError(t, err)
			require.Equal(t, !zero, strings.Contains(string(encoded), `"description"`))
		}
		followup := map[string]any{"input": []any{}}
		_, _, err = AdaptResponsesClientToolsWithInheritedMappingAndPromptPolicy(followup, mapping, zero, lowered)
		require.NoError(t, err)
		followupTools, ok := followup["tools"].([]any)
		require.True(t, ok)
		require.Len(t, followupTools, 2)
		firstFollowupTool, ok := followupTools[0].(map[string]any)
		require.True(t, ok)
		require.Equal(t, callerDescription, firstFollowupTool["description"])
		encoded, err := json.Marshal(followup)
		require.NoError(t, err)
		require.Equal(t, !zero, strings.Contains(string(encoded), "raw input for this tool"))
	}
}

func TestZeroPromptInjectionChatAndAnthropicEmptyOutputs(t *testing.T) {
	request := &ChatCompletionsRequest{Model: "gpt-6.2-sol", Messages: []ChatMessage{
		{Role: "assistant", ToolCalls: []ChatToolCall{{ID: "empty", Type: "function", Function: ChatFunctionCall{Name: "lookup", Arguments: "{}"}}}},
		{Role: "tool", ToolCallID: "empty", Content: json.RawMessage(`""`)},
		{Role: "assistant", FunctionCall: &ChatFunctionCall{Name: "legacy", Arguments: "{}"}},
		{Role: "function", Name: "legacy", Content: json.RawMessage(`""`)},
	}}
	for _, zero := range []bool{false, true} {
		responses, err := ChatCompletionsToResponsesWithPromptPolicy(request, 0, zero)
		require.NoError(t, err)
		var input []ResponsesInputItem
		require.NoError(t, json.Unmarshal(responses.Input, &input))
		want := "(empty)"
		if zero {
			want = ""
		}
		require.Equal(t, want, input[1].Output)
		require.Equal(t, want, input[3].Output)
		responses.Instructions = "  exact instruction\n"
		anthropic, err := ResponsesToAnthropicRequestWithPromptPolicy(responses, zero)
		require.NoError(t, err)
		for _, message := range anthropic.Messages {
			var blocks []AnthropicContentBlock
			require.NoError(t, json.Unmarshal(message.Content, &blocks))
			for _, block := range blocks {
				if block.Type == "tool_result" {
					var output string
					require.NoError(t, json.Unmarshal(block.Content, &output))
					require.Equal(t, want, output)
				}
			}
		}
		if zero {
			var system string
			require.NoError(t, json.Unmarshal(anthropic.System, &system))
			require.Equal(t, responses.Instructions, system)
		}
	}
}

func TestZeroPromptInjectionEmptyToolResultsRemainEmpty(t *testing.T) {
	request := &AnthropicRequest{Model: "gpt-6.2-sol", Messages: []AnthropicMessage{
		{Role: "assistant", Content: json.RawMessage(`[{"type":"tool_use","id":"empty","name":"lookup","input":{}},{"type":"tool_use","id":"literal","name":"lookup","input":{}}]`)},
		{Role: "user", Content: json.RawMessage(`[{"type":"tool_result","tool_use_id":"empty","content":""},{"type":"tool_result","tool_use_id":"literal","content":"(empty)"}]`)},
	}}
	for _, zero := range []bool{false, true} {
		responses, err := AnthropicToResponsesWithOptions(request, AnthropicToResponsesOptions{ZeroPromptInjection: zero})
		require.NoError(t, err)
		var input []ResponsesInputItem
		require.NoError(t, json.Unmarshal(responses.Input, &input))
		want := "(empty)"
		if zero {
			want = ""
		}
		require.Equal(t, want, input[2].Output)
		require.Equal(t, "(empty)", input[3].Output)
		chat, err := AnthropicToChatCompletionsRequestWithPromptPolicy(request, 0, zero)
		require.NoError(t, err)
		var output string
		require.NoError(t, json.Unmarshal(chat.Messages[1].Content, &output))
		require.Equal(t, want, output)
		require.NoError(t, json.Unmarshal(chat.Messages[2].Content, &output))
		require.Equal(t, "(empty)", output)
	}
}
