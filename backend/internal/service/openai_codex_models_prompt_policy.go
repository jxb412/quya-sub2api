package service

import (
	"bytes"
	"encoding/json"
	"fmt"
)

// 2026-11-26: a zero-addition group must not advertise our bundled prompts back
// to Codex, which would otherwise send them as client instructions next turn.
// Upstream-supplied prompts are outside this policy and retain their provenance.
func applyCodexManifestPromptPolicy(body, upstreamSource []byte) ([]byte, bool, error) {
	var envelope map[string]json.RawMessage
	if err := json.Unmarshal(body, &envelope); err != nil {
		return nil, false, err
	}
	var models []json.RawMessage
	if err := json.Unmarshal(envelope["models"], &models); err != nil {
		return nil, false, err
	}
	sources := make(map[string]map[string]json.RawMessage)
	if len(upstreamSource) > 0 {
		var source struct {
			Models []map[string]json.RawMessage `json:"models"`
		}
		if err := json.Unmarshal(upstreamSource, &source); err != nil {
			return nil, false, fmt.Errorf("decode prompt provenance: %w", err)
		}
		for _, model := range source.Models {
			var slug string
			if json.Unmarshal(model["slug"], &slug) == nil && slug != "" {
				sources[slug] = model
			}
		}
	}
	changed := false
	for i, raw := range models {
		var model map[string]json.RawMessage
		if err := json.Unmarshal(raw, &model); err != nil {
			return nil, false, err
		}
		var slug string
		if json.Unmarshal(model["slug"], &slug) != nil || slug == "" {
			continue
		}
		source := sources[slug]
		emptyMessages, _ := json.Marshal(configuredCodexModelMessages{})
		var messages map[string]json.RawMessage
		_ = json.Unmarshal(emptyMessages, &messages)
		if rawSource := source["model_messages"]; len(rawSource) > 0 && !bytes.Equal(bytes.TrimSpace(rawSource), []byte("null")) {
			var sourceMessages map[string]json.RawMessage
			if err := json.Unmarshal(rawSource, &sourceMessages); err != nil {
				return nil, false, fmt.Errorf("decode upstream model messages: %w", err)
			}
			for key, value := range sourceMessages {
				messages[key] = value
			}
		}
		encodedMessages, err := json.Marshal(messages)
		if err != nil {
			return nil, false, err
		}
		model["model_messages"] = encodedMessages
		for _, key := range []string{"base_instructions", "include_skills_usage_instructions", "include_plugin_usage_instructions", "include_apps_usage_instructions"} {
			if value, ok := source[key]; ok {
				model[key] = value
			} else if _, exists := model[key]; exists {
				if key == "base_instructions" {
					model[key] = json.RawMessage(`""`)
				} else {
					model[key] = json.RawMessage("false")
				}
			}
		}
		encoded, err := json.Marshal(model)
		if err != nil {
			return nil, false, err
		}
		if !bytes.Equal(raw, encoded) {
			models[i] = encoded
			changed = true
		}
	}
	if !changed {
		return body, false, nil
	}
	envelope["models"], _ = json.Marshal(models)
	encoded, err := json.Marshal(envelope)
	return encoded, true, err
}

func applyGeneratedCodexManifestPromptPolicy(body []byte, group *Group) ([]byte, error) {
	if group == nil || group.Platform != PlatformOpenAI || !group.OpenAIZeroPromptInjection {
		return body, nil
	}
	body, _, err := applyCodexManifestPromptPolicy(body, nil)
	return body, err
}
