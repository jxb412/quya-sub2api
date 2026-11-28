package service

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
)

func nonStreamRecoveryFrames(events ...string) string {
	return "data: " + strings.Join(events, "\n\ndata: ") + "\n\n"
}

// 2026-11-28 regression: a completed reasoning item must not hide a later
// unfinished message when the upstream omits response.completed.
func TestRecoverOpenAINonStreamingOutputMergesDoneAndPendingItems(t *testing.T) {
	reasoning := `{"id":"rs_1","type":"reasoning","encrypted_content":"opaque","summary":[],"vendor_extension":{"keep":true}}`
	doneMessage := `{"id":"msg_done","type":"message","status":"completed","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"First","annotations":[{"type":"url_citation","url":"https://example.com"}]}],"future_field":[1,2]}`
	body := nonStreamRecoveryFrames(
		`{"type":"response.created","response":{"id":"resp_upstream","model":"upstream-model","created_at":1795800000}}`,
		`{"type":"response.output_item.done","output_index":0,"item":`+reasoning+`}`,
		`{"type":"response.output_text.delta","output_index":2,"item_id":"msg_partial","content_index":0,"delta":"Last "}`,
		`{"type":"response.output_item.done","output_index":1,"item":`+doneMessage+`}`,
		`{"type":"response.output_text.delta","output_index":2,"item_id":"msg_partial","content_index":0,"delta":"answer"}`,
	)
	response, upstreamID, ok := recoverOpenAINonStreamingOutput(body, "fallback-model")
	require.True(t, ok)
	require.Equal(t, "resp_upstream", upstreamID)
	require.Equal(t, "resp_upstream", gjson.GetBytes(response, "id").String())
	require.Equal(t, "response", gjson.GetBytes(response, "object").String())
	require.Equal(t, "upstream-model", gjson.GetBytes(response, "model").String())
	require.Equal(t, int64(1795800000), gjson.GetBytes(response, "created_at").Int())
	require.Equal(t, "incomplete", gjson.GetBytes(response, "status").String())
	require.False(t, gjson.GetBytes(response, "usage").Exists())
	require.Equal(t, "null", gjson.GetBytes(response, "incomplete_details").Raw)
	require.Equal(t, "null", gjson.GetBytes(response, "error").Raw)
	output := gjson.GetBytes(response, "output").Array()
	require.Len(t, output, 3)
	require.JSONEq(t, reasoning, output[0].Raw)
	require.JSONEq(t, doneMessage, output[1].Raw)
	require.Equal(t, "msg_partial", output[2].Get("id").String())
	require.Equal(t, "Last answer", output[2].Get("content.0.text").String())
	require.Equal(t, "incomplete", output[2].Get("status").String())
}

func TestRecoverOpenAINonStreamingOutputPartDoneWinsWithoutDuplicatingDeltas(t *testing.T) {
	body := nonStreamRecoveryFrames(
		`{"type":"response.output_item.added","output_index":0,"item":{"id":"msg_1","type":"message","status":"in_progress","content":[],"phase":"final_answer","opaque_extension":"preserved"}}`,
		`{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"hel"}`,
		`{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"lo"}`,
		`{"type":"response.output_text.done","output_index":0,"content_index":0,"text":"Hello!"}`,
		`{"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":"Hello!","annotations":[{"type":"custom","value":3}],"extension":"keep"}}`,
		`{"type":"response.output_text.delta","output_index":0,"content_index":1,"delta":"Second part"}`,
	)
	response, _, ok := recoverOpenAINonStreamingOutput(body, "model")
	require.True(t, ok)
	item := gjson.GetBytes(response, "output.0")
	require.Equal(t, "Hello!", item.Get("content.0.text").String())
	require.Equal(t, "Second part", item.Get("content.1.text").String())
	require.Len(t, item.Get("content").Array(), 2)
	require.Equal(t, "keep", item.Get("content.0.extension").String())
	require.Equal(t, int64(3), item.Get("content.0.annotations.0.value").Int())
	require.Equal(t, "preserved", item.Get("opaque_extension").String())
	require.Equal(t, "final_answer", item.Get("phase").String())
}

func TestRecoverOpenAINonStreamingOutputDoesNotInventUpstreamID(t *testing.T) {
	response, upstreamID, ok := recoverOpenAINonStreamingOutput(nonStreamRecoveryFrames(
		`{"type":"response.output_text.delta","output_index":0,"delta":"中文正文"}`,
	), "fallback-model")
	require.True(t, ok)
	require.Empty(t, upstreamID)
	require.True(t, strings.HasPrefix(gjson.GetBytes(response, "id").String(), "resp_"))
	require.Equal(t, "fallback-model", gjson.GetBytes(response, "model").String())
	require.Positive(t, gjson.GetBytes(response, "created_at").Int())
	require.Equal(t, "中文正文", gjson.GetBytes(response, "output.0.content.0.text").String())
}

func TestRecoverOpenAINonStreamingOutputRejectsEmptyOrUnsafeOnlyStreams(t *testing.T) {
	cases := map[string]string{
		"empty":                "",
		"heartbeats":           ": heartbeat\n\ndata: [DONE]\n\n",
		"created only":         nonStreamRecoveryFrames(`{"type":"response.created","response":{"id":"resp_empty","status":"in_progress","output":[]}}`),
		"opaque only":          nonStreamRecoveryFrames(`{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","encrypted_content":"opaque","summary":[]}}`),
		"whitespace":           nonStreamRecoveryFrames(`{"type":"response.output_text.delta","delta":"  \n "}`),
		"broken JSON":          "data: {\"type\":\"response.output_text.delta\",\"delta\":\"cut",
		"partial tool":         nonStreamRecoveryFrames(`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","name":"danger","call_id":"call_1","arguments":""}}`, `{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"x\":"}`),
		"args done only":       nonStreamRecoveryFrames(`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","name":"tool","call_id":"call_1","arguments":""}}`, `{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{}"}`),
		"invalid done args":    nonStreamRecoveryFrames(`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"tool","call_id":"call_1","arguments":"{broken"}}`),
		"incomplete done tool": nonStreamRecoveryFrames(`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","status":"incomplete","name":"tool","call_id":"call_1","arguments":"{}"}}`),
	}
	for name, body := range cases {
		t.Run(name, func(t *testing.T) {
			response, upstreamID, ok := recoverOpenAINonStreamingOutput(body, "model")
			require.False(t, ok)
			require.Empty(t, response)
			require.Empty(t, upstreamID)
		})
	}
}

func TestRecoverOpenAINonStreamingOutputPreservesCompleteToolAndDropsPartialTool(t *testing.T) {
	tool := `{"id":"fc_done","type":"function_call","status":"completed","call_id":"call_done","name":"search","arguments":"{\"query\":\"ok\"}","namespace":"tools","encrypted_content":"keep"}`
	body := nonStreamRecoveryFrames(
		`{"type":"response.output_item.done","output_index":0,"item":`+tool+`}`,
		`{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_partial","call_id":"call_partial","name":"run","arguments":""}}`,
		`{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"command\":"}`,
	)
	response, _, ok := recoverOpenAINonStreamingOutput(body, "model")
	require.True(t, ok)
	require.Len(t, gjson.GetBytes(response, "output").Array(), 1)
	require.JSONEq(t, tool, gjson.GetBytes(response, "output.0").Raw)
}

func TestRecoverOpenAINonStreamingOutputTextSurvivesBrokenTool(t *testing.T) {
	body := nonStreamRecoveryFrames(
		`{"type":"response.output_text.done","output_index":0,"text":"Preserve this answer"}`,
		`{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","name":"tool","call_id":"call_1","arguments":"{"}}`,
	)
	response, _, ok := recoverOpenAINonStreamingOutput(body, "model")
	require.True(t, ok)
	require.Len(t, gjson.GetBytes(response, "output").Array(), 1)
	require.Equal(t, "Preserve this answer", gjson.GetBytes(response, "output.0.content.0.text").String())
}

func TestRecoverOpenAINonStreamingOutputSupportsEventNamesAndMultilineData(t *testing.T) {
	body := "event: response.content_part.done\r\ndata: {\"output_index\":0,\r\ndata: \"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"complete part\"}}\r\n\r\n"
	response, _, ok := recoverOpenAINonStreamingOutput(body, "model")
	require.True(t, ok)
	require.Equal(t, "complete part", gjson.GetBytes(response, "output.0.content.0.text").String())
}

func TestRecoverOpenAINonStreamingOutputIndexFreeTextDoesNotAttachToReasoning(t *testing.T) {
	body := nonStreamRecoveryFrames(
		`{"type":"response.output_item.done","item":{"id":"rs_1","type":"reasoning","encrypted_content":"keep"}}`,
		`{"type":"response.output_text.delta","item_id":"msg_1","delta":"answer"}`,
		`{"type":"response.output_text.done","item_id":"msg_1","text":"answer!"}`,
	)
	response, _, ok := recoverOpenAINonStreamingOutput(body, "model")
	require.True(t, ok)
	require.Len(t, gjson.GetBytes(response, "output").Array(), 2)
	require.Equal(t, "reasoning", gjson.GetBytes(response, "output.0.type").String())
	require.Equal(t, "answer!", gjson.GetBytes(response, "output.1.content.0.text").String())
}

func TestRecoverOpenAINonStreamingOutputKeepsContentExtensionsOnRawDoneMessage(t *testing.T) {
	item := `{"id":"msg_done","type":"message","role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]},{"type":"future_output_part","opaque":"preserve"}],"status":"completed"}`
	response, _, ok := recoverOpenAINonStreamingOutput(nonStreamRecoveryFrames(`{"type":"response.output_item.done","output_index":0,"item":`+item+`}`), "model")
	require.True(t, ok)
	require.JSONEq(t, item, gjson.GetBytes(response, "output.0").Raw)
	var top map[string]json.RawMessage
	require.NoError(t, json.Unmarshal(response, &top))
	require.Len(t, top, 8, "must not add private recovery/billing metadata to client JSON")
}

func TestRecoverOpenAINonStreamingOutputDeduplicatesDoneAndKeepsLaterText(t *testing.T) {
	completed := `{"type":"response.output_item.done","output_index":0,"item":{"id":"msg_first","type":"message","content":[{"type":"output_text","text":"first"}],"status":"completed"}}`
	body := nonStreamRecoveryFrames(
		completed, completed,
		`{"type":"response.output_item.added","output_index":1,"item":{"id":"msg_next","type":"message","content":[{"type":"output_text","text":"new "}]}}`,
		`{"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"answer"}`,
	)
	response, _, ok := recoverOpenAINonStreamingOutput(body, "model")
	require.True(t, ok)
	require.Len(t, gjson.GetBytes(response, "output").Array(), 2)
	require.Equal(t, "first", gjson.GetBytes(response, "output.0.content.0.text").String())
	require.Equal(t, "new answer", gjson.GetBytes(response, "output.1.content.0.text").String())
}

func TestRecoverOpenAINonStreamingOutputRefusalAndImageResults(t *testing.T) {
	body := nonStreamRecoveryFrames(
		`{"type":"response.refusal.delta","response_id":"resp_known","output_index":0,"delta":"Cannot "}`,
		`{"type":"response.refusal.done","output_index":0,"refusal":"Cannot do that."}`,
		`{"type":"response.output_item.done","output_index":1,"item":{"type":"image_generation_call","id":"img_1","status":"completed","result":"aW1hZ2U=","output_format":"png"}}`,
		`{"type":"response.output_item.done","output_index":2,"item":{"type":"image_generation_call","id":"img_empty","result":""}}`,
	)
	response, upstreamID, ok := recoverOpenAINonStreamingOutput(body, "model")
	require.True(t, ok)
	require.Equal(t, "resp_known", upstreamID)
	require.Len(t, gjson.GetBytes(response, "output").Array(), 2)
	require.Equal(t, "refusal", gjson.GetBytes(response, "output.0.content.0.type").String())
	require.Equal(t, "Cannot do that.", gjson.GetBytes(response, "output.0.content.0.refusal").String())
	require.Equal(t, "aW1hZ2U=", gjson.GetBytes(response, "output.1.result").String())
}

func TestRecoverOpenAINonStreamingOutputPreservesAddedTextPrefix(t *testing.T) {
	cases := map[string]string{
		"item snapshot": `{"type":"response.output_item.added","output_index":0,"item":{"id":"msg_1","type":"message","content":[{"type":"output_text","text":"hello "}]}}`,
		"part snapshot": `{"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":"hello "}}`,
	}
	for name, initial := range cases {
		t.Run(name, func(t *testing.T) {
			body := nonStreamRecoveryFrames(initial, `{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"world"}`)
			response, _, ok := recoverOpenAINonStreamingOutput(body, "model")
			require.True(t, ok)
			require.Equal(t, "hello world", gjson.GetBytes(response, "output.0.content.0.text").String())
			body += nonStreamRecoveryFrames(`{"type":"response.output_text.done","output_index":0,"content_index":0,"text":"Hello world!"}`)
			response, _, ok = recoverOpenAINonStreamingOutput(body, "model")
			require.True(t, ok)
			require.Equal(t, "Hello world!", gjson.GetBytes(response, "output.0.content.0.text").String())
		})
	}
}
