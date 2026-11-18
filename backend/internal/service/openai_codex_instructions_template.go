package service

import (
	"bytes"
	"fmt"
	"strings"
	"text/template"
)

type forcedCodexInstructionsTemplateData struct {
	ExistingInstructions string
	OriginalModel        string
	NormalizedModel      string
	BillingModel         string
	UpstreamModel        string
}

func applyForcedCodexInstructionsTemplate(
	reqBody map[string]any,
	templateText string,
	data forcedCodexInstructionsTemplateData,
) (bool, error) {
	if useCodexStableInstructionPrefix(data.UpstreamModel) {
		return applyForcedCodexStableInstructionsTemplate(reqBody, templateText, data)
	}
	rendered, err := renderForcedCodexInstructionsTemplate(templateText, data)
	if err != nil {
		return false, err
	}
	if rendered == "" {
		return false, nil
	}

	existing, _ := reqBody["instructions"].(string)
	if strings.TrimSpace(existing) == rendered {
		return false, nil
	}

	reqBody["instructions"] = rendered
	return true, nil
}

// Messages conversion owns its leading developer messages. On GPT-6.2, render
// the administrator's stable template in that cacheable prefix, without copying
// later developer turns or the client's turn-scoped instructions into it.
func applyForcedCodexStableInstructionsTemplate(reqBody map[string]any, templateText string, data forcedCodexInstructionsTemplateData) (bool, error) {
	if strings.TrimSpace(templateText) == "" || hasCodexInstructionContinuation(reqBody) {
		return false, nil
	}
	input, _ := reqBody["input"].([]any)
	prefixCount := 0
	var texts []string
	for _, item := range input {
		message, ok := item.(map[string]any)
		if !ok || message["role"] != "developer" {
			break
		}
		text, lossless := extractLosslessTextFromContent(message["content"])
		if !lossless {
			break
		}
		texts = append(texts, text)
		prefixCount++
	}
	data.ExistingInstructions = strings.Join(texts, "\n\n")
	rendered, err := renderForcedCodexInstructionsTemplate(templateText, data)
	if err != nil || rendered == "" {
		return false, err
	}
	if prefixCount == 0 || (data.ExistingInstructions != "" && !strings.Contains(rendered, data.ExistingInstructions)) {
		// A template without ExistingInstructions must not erase the client's
		// system guidance, which the old path also retained in input.
		return prependCodexStableInstructions(reqBody, rendered), nil
	}
	if prefixCount == 1 && texts[0] == rendered {
		return false, nil
	}
	first, ok := input[0].(map[string]any)
	if !ok {
		return false, nil
	}
	replacement := make(map[string]any, len(first))
	for key, value := range first {
		replacement[key] = value
	}
	replacement["content"] = []any{map[string]any{"type": "input_text", "text": rendered}}
	reqBody["input"] = append([]any{replacement}, input[prefixCount:]...)
	return true, nil
}

func renderForcedCodexInstructionsTemplate(
	templateText string,
	data forcedCodexInstructionsTemplateData,
) (string, error) {
	tmpl, err := template.New("forced_codex_instructions").Option("missingkey=zero").Parse(templateText)
	if err != nil {
		return "", fmt.Errorf("parse forced codex instructions template: %w", err)
	}

	var buf bytes.Buffer
	if err := tmpl.Execute(&buf, data); err != nil {
		return "", fmt.Errorf("render forced codex instructions template: %w", err)
	}

	return strings.TrimSpace(buf.String()), nil
}
