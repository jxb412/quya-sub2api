package service

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
)

func TestZeroPromptInjectionImagesBridge(t *testing.T) {
	const prompt = "  Customer's exact image prompt.\n"
	body, err := json.Marshal(map[string]string{"prompt": prompt})
	require.NoError(t, err)
	parsed := &OpenAIImagesRequest{Prompt: "Customer's exact image prompt.", Body: body}
	for _, zero := range []bool{false, true} {
		body, err := buildOpenAIImagesResponsesRequestWithPromptPolicy(parsed, "gpt-image-2", zero)
		require.NoError(t, err)
		instructions := gjson.GetBytes(body, "instructions")
		require.True(t, instructions.Exists())
		if zero {
			require.Empty(t, instructions.String())
			require.Equal(t, prompt, gjson.GetBytes(body, "input.0.content.0.text").String())
		} else {
			require.Equal(t, openAIImagesVerbatimPromptInstructions, instructions.String())
		}
		require.Equal(t, "image_generation", gjson.GetBytes(body, "tool_choice.type").String())
		require.Equal(t, "gpt-image-2", gjson.GetBytes(body, "tools.0.model").String())
	}
}

func TestZeroPromptInjectionGeneratedCodexManifestAndETag(t *testing.T) {
	repo := codexModelsVisibilityAccountRepo{byGroup: map[int64][]Account{71: {{ID: 1, Platform: PlatformOpenAI, Type: AccountTypeOAuth, Status: StatusActive, Schedulable: true, Credentials: map[string]any{"model_mapping": map[string]any{"gpt-6.2-sol": "gpt-6.2-sol"}}}}}}
	svc := &OpenAIGatewayService{accountRepo: repo}
	group := &Group{ID: 71, Platform: PlatformOpenAI, ModelAllowlist: GroupModelAllowlist{Enabled: true, Models: []string{"gpt-6.2-sol"}}}
	normal, configured, err := svc.BuildGroupConfiguredCodexModelsManifest(context.Background(), group, "")
	require.NoError(t, err)
	require.True(t, configured)
	require.NotEmpty(t, gjson.GetBytes(normal.Body, "models.0.model_messages.instructions_template").String())
	group.OpenAIZeroPromptInjection = true
	strict, configured, err := svc.BuildGroupConfiguredCodexModelsManifest(context.Background(), group, normal.ETag)
	require.NoError(t, err)
	require.True(t, configured)
	require.False(t, strict.NotModified)
	require.NotEqual(t, normal.ETag, strict.ETag)
	require.Empty(t, gjson.GetBytes(strict.Body, "models.0.model_messages.instructions_template").String())
	require.Empty(t, gjson.GetBytes(strict.Body, "models.0.base_instructions").String())
	require.False(t, gjson.GetBytes(strict.Body, "models.0.model_messages.tools").Exists())
	require.Equal(t, gjson.GetBytes(normal.Body, "models.0.context_window").Int(), gjson.GetBytes(strict.Body, "models.0.context_window").Int())
	again, _, err := svc.BuildGroupConfiguredCodexModelsManifest(context.Background(), group, strict.ETag)
	require.NoError(t, err)
	require.True(t, again.NotModified)
}

func TestZeroPromptInjectionCodexManifestPreservesUpstreamProvenance(t *testing.T) {
	source := []byte(`{"models":[{"slug":"vendor-model","model_messages":{"instructions_template":"vendor supplied","tools":{"hint":"vendor tool hint"}},"base_instructions":"vendor base","input_modalities":["text","image"]}]}`)
	completed := []byte(`{"models":[{"slug":"vendor-model","model_messages":{"instructions_template":"vendor supplied","tools":{"hint":"vendor tool hint"},"permissions":{"hint":"local permission hint"}},"base_instructions":"vendor base","include_apps_usage_instructions":true,"input_modalities":["text","image"]}]}`)
	before := append([]byte(nil), completed...)
	manifest := &OpenAIModelsResponse{Body: completed, upstreamSourceBody: source}
	group := &Group{Platform: PlatformOpenAI, OpenAIZeroPromptInjection: true}
	account := &Account{Platform: PlatformOpenAI, Type: AccountTypeAPIKey}
	require.NoError(t, ApplyPinnedCodexModelsMapping(manifest, account, group))
	require.Equal(t, "vendor supplied", gjson.GetBytes(manifest.Body, "models.0.model_messages.instructions_template").String())
	require.Equal(t, "vendor tool hint", gjson.GetBytes(manifest.Body, "models.0.model_messages.tools.hint").String())
	require.Equal(t, "vendor base", gjson.GetBytes(manifest.Body, "models.0.base_instructions").String())
	require.NotContains(t, string(manifest.Body), "local permission hint")
	require.False(t, gjson.GetBytes(manifest.Body, "models.0.include_apps_usage_instructions").Bool())
	require.JSONEq(t, string(source), string(manifest.upstreamSourceBody))
	require.Equal(t, before, completed)
	merged, changed, err := mergeConfiguredCodexModelsManifestWithPromptPolicy(manifest.Body, []string{"custom-alias"}, nil, false, true)
	require.NoError(t, err)
	require.True(t, changed)
	require.Equal(t, "vendor supplied", gjson.GetBytes(merged, "models.0.model_messages.instructions_template").String())
	require.Empty(t, gjson.GetBytes(merged, "models.1.model_messages.instructions_template").String())
}
