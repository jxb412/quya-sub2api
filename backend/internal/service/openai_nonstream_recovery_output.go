package service

import (
	"encoding/json"
	"sort"
	"strings"
	"time"

	"github.com/google/uuid"
	"github.com/tidwall/gjson"
)

// 2026-11-28: An upstream may close after delivering output but before its final
// response event. Recover that output without inventing completion, usage, tool
// arguments, or an upstream response ID. Keep raw items so encrypted content and
// vendor extensions survive; callers decide billing and whether to bind an ID.
func recoverOpenAINonStreamingOutput(bodyText, model string) (response []byte, upstreamResponseID string, ok bool) {
	var slots []*openAINonStreamRecoverySlot
	indexed := make(map[int64]*openAINonStreamRecoverySlot)
	byID := make(map[string]*openAINonStreamRecoverySlot)
	createdAt := time.Now().Unix()
	getSlot := func(event gjson.Result, itemType, itemID string) *openAINonStreamRecoverySlot {
		index := event.Get("output_index")
		hasIndex := index.Type == gjson.Number && index.Int() >= 0 && index.Float() == float64(index.Int())
		var slot *openAINonStreamRecoverySlot
		if hasIndex {
			slot = indexed[index.Int()]
		}
		if slot == nil && itemID != "" {
			slot = byID[itemID]
		}
		if slot == nil && !hasIndex && itemID == "" {
			// Index-free legacy text events can extend the most recent message,
			// but must never attach themselves to a completed reasoning item.
			for i := len(slots) - 1; i >= 0; i-- {
				if slots[i].itemType == itemType && slots[i].done == nil {
					slot = slots[i]
					break
				}
			}
		}
		if slot == nil {
			slot = &openAINonStreamRecoverySlot{order: len(slots), itemType: itemType, parts: make(map[int64]*openAINonStreamRecoveryPart)}
			slots = append(slots, slot)
		}
		if hasIndex {
			slot.index, slot.indexed = index.Int(), true
			indexed[slot.index] = slot
		}
		if itemID != "" {
			slot.id = itemID
			byID[itemID] = slot
		}
		return slot
	}
	forEachOpenAISSEFrame(bodyText, func(eventType string, data []byte) {
		if !gjson.ValidBytes(data) {
			return
		}
		event := gjson.ParseBytes(data)
		if metadata := event.Get("response"); metadata.IsObject() {
			if id := strings.TrimSpace(metadata.Get("id").String()); id != "" {
				upstreamResponseID = id
			}
			if observedModel := strings.TrimSpace(metadata.Get("model").String()); observedModel != "" {
				model = observedModel
			}
			if timestamp := metadata.Get("created_at"); timestamp.Type == gjson.Number && timestamp.Int() > 0 {
				createdAt = timestamp.Int()
			}
		}
		if id := strings.TrimSpace(event.Get("response_id").String()); id != "" && upstreamResponseID == "" {
			upstreamResponseID = id
		}
		switch eventType {
		case "response.output_item.added", "response.output_item.done":
			item := event.Get("item")
			if !item.IsObject() {
				return
			}
			slot := getSlot(event, item.Get("type").String(), item.Get("id").String())
			if eventType == "response.output_item.done" {
				slot.done = json.RawMessage(item.Raw)
			} else {
				slot.added = json.RawMessage(item.Raw)
			}
		case "response.output_text.delta", "response.output_text.done", "response.refusal.delta", "response.refusal.done", "response.content_part.added", "response.content_part.done":
			slot := getSlot(event, "message", event.Get("item_id").String())
			index := event.Get("content_index").Int()
			if index < 0 {
				return
			}
			part := slot.parts[index]
			if part == nil {
				part = &openAINonStreamRecoveryPart{}
				slot.parts[index] = part
			}
			switch eventType {
			case "response.output_text.delta", "response.refusal.delta":
				part.kind = "output_text"
				if eventType == "response.refusal.delta" {
					part.kind = "refusal"
				}
				_, _ = part.delta.WriteString(event.Get("delta").String())
			case "response.output_text.done", "response.refusal.done":
				part.kind, part.complete = "output_text", event.Get("text").String()
				if eventType == "response.refusal.done" {
					part.kind, part.complete = "refusal", event.Get("refusal").String()
				}
			case "response.content_part.added", "response.content_part.done":
				if raw := event.Get("part"); raw.IsObject() {
					if eventType == "response.content_part.done" {
						part.done = json.RawMessage(raw.Raw)
					} else {
						part.added = json.RawMessage(raw.Raw)
					}
				}
			}
		}
	})
	sort.SliceStable(slots, func(i, j int) bool {
		if slots[i].indexed && slots[j].indexed {
			return slots[i].index < slots[j].index
		}
		if slots[i].indexed != slots[j].indexed {
			return slots[i].indexed
		}
		return slots[i].order < slots[j].order
	})
	var output []json.RawMessage
	deliverable := false
	for _, slot := range slots {
		item, semantic := slot.build()
		if len(item) != 0 {
			output = append(output, item)
		}
		deliverable = deliverable || semantic
	}
	// Opaque reasoning, created/heartbeat frames and partial tool arguments are
	// not an answer. Do not turn them into a successful, billable empty result.
	if !deliverable {
		return nil, "", false
	}
	id := upstreamResponseID
	if id == "" {
		id = "resp_" + strings.ReplaceAll(uuid.NewString(), "-", "")
	}
	response, err := json.Marshal(map[string]any{
		"id": id, "object": "response", "created_at": createdAt,
		"model": model, "status": "incomplete", "output": output,
		"incomplete_details": nil, "error": nil,
	})
	return response, upstreamResponseID, err == nil
}

type openAINonStreamRecoveryPart struct {
	kind     string
	delta    strings.Builder
	complete string
	added    json.RawMessage
	done     json.RawMessage
}

type openAINonStreamRecoverySlot struct {
	index    int64
	indexed  bool
	order    int
	id       string
	itemType string
	added    json.RawMessage
	done     json.RawMessage
	parts    map[int64]*openAINonStreamRecoveryPart
}

func openAINonStreamRecoveryText(raw json.RawMessage) bool {
	p := gjson.ParseBytes(raw)
	switch p.Get("type").String() {
	case "output_text":
		return strings.TrimSpace(p.Get("text").String()) != ""
	case "refusal":
		return strings.TrimSpace(p.Get("refusal").String()) != ""
	}
	return false
}

func (p *openAINonStreamRecoveryPart) build(existing json.RawMessage, authoritative bool) json.RawMessage {
	if authoritative && openAINonStreamRecoveryText(existing) {
		return existing
	}
	if openAINonStreamRecoveryText(p.done) {
		return p.done
	}
	text := p.complete
	if strings.TrimSpace(text) == "" {
		// Added items/parts can already contain a nonempty prefix. Deltas are
		// suffixes to that snapshot; only done events replace the whole text.
		for _, initial := range []json.RawMessage{p.added, existing} {
			if openAINonStreamRecoveryText(initial) {
				field := "text"
				if gjson.GetBytes(initial, "type").String() == "refusal" {
					field = "refusal"
				}
				text = gjson.GetBytes(initial, field).String()
				break
			}
		}
		text += p.delta.String()
	}
	if strings.TrimSpace(text) == "" {
		if openAINonStreamRecoveryText(p.added) {
			return p.added
		}
		return nil
	}
	base := make(map[string]json.RawMessage)
	for _, raw := range []json.RawMessage{p.added, p.done, existing} {
		if len(raw) > 0 {
			_ = json.Unmarshal(raw, &base)
		}
	}
	kind := p.kind
	if kind == "" {
		kind = gjson.ParseBytes(base["type"]).String()
		if kind != "refusal" {
			kind = "output_text"
		}
	}
	base["type"], _ = json.Marshal(kind)
	field := "text"
	if kind == "refusal" {
		field = "refusal"
	} else if _, exists := base["annotations"]; !exists {
		base["annotations"] = json.RawMessage(`[]`)
	}
	base[field], _ = json.Marshal(text)
	result, _ := json.Marshal(base)
	return result
}

func (s *openAINonStreamRecoverySlot) build() (json.RawMessage, bool) {
	raw := s.done
	if raw == nil {
		raw = s.added
	}
	item := gjson.ParseBytes(raw)
	itemType := item.Get("type").String()
	if itemType == "" {
		itemType = s.itemType
	}
	if itemType != "message" {
		// Tool argument deltas, even valid JSON prefixes, do not prove the call
		// completed. Only output_item.done is safe to expose as an executable call.
		if s.done == nil {
			return nil, false
		}
		switch itemType {
		case "function_call", "custom_tool_call":
			if strings.TrimSpace(item.Get("call_id").String()) == "" || strings.TrimSpace(item.Get("name").String()) == "" {
				return nil, false
			}
			if status := item.Get("status").String(); status != "" && status != "completed" {
				return nil, false
			}
			if itemType == "function_call" {
				arguments := item.Get("arguments")
				if arguments.Type != gjson.String || !json.Valid([]byte(arguments.String())) {
					return nil, false
				}
			} else if item.Get("input").Type != gjson.String {
				return nil, false
			}
			return raw, true
		case "image_generation_call":
			if strings.TrimSpace(item.Get("result").String()) != "" {
				return raw, true
			}
			return nil, false
		default:
			// Keep completed reasoning/hosted item extensions alongside real
			// output, but never use opaque state alone to justify recovery.
			return raw, false
		}
	}
	content := make(map[int64]json.RawMessage)
	for index, part := range item.Get("content").Array() {
		content[int64(index)] = json.RawMessage(part.Raw)
	}
	for index, part := range s.parts {
		if recovered := part.build(content[index], s.done != nil); len(recovered) != 0 {
			content[index] = recovered
		}
	}
	indexes := make([]int64, 0, len(content))
	for index := range content {
		indexes = append(indexes, index)
	}
	sort.Slice(indexes, func(i, j int) bool { return indexes[i] < indexes[j] })
	parts := make([]json.RawMessage, 0, len(indexes))
	semantic := false
	for _, index := range indexes {
		if usable := openAINonStreamRecoveryText(content[index]); usable || s.done != nil {
			parts = append(parts, content[index])
			semantic = semantic || usable
		}
	}
	if len(parts) == 0 {
		return nil, false
	}
	base := make(map[string]json.RawMessage)
	if len(raw) > 0 {
		_ = json.Unmarshal(raw, &base)
	}
	base["type"] = json.RawMessage(`"message"`)
	if _, exists := base["role"]; !exists {
		base["role"] = json.RawMessage(`"assistant"`)
	}
	if _, exists := base["id"]; !exists {
		id := s.id
		if id == "" {
			id = "msg_" + strings.ReplaceAll(uuid.NewString(), "-", "")
		}
		base["id"], _ = json.Marshal(id)
	}
	if s.done == nil {
		base["status"] = json.RawMessage(`"incomplete"`)
	}
	base["content"], _ = json.Marshal(parts)
	result, err := json.Marshal(base)
	return result, err == nil && semantic
}
