package service

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestAPIKeyAuthSnapshotZeroPromptInjectionRoundtrip(t *testing.T) {
	for _, enabled := range []bool{false, true} {
		groupID := int64(57)
		apiKey := &APIKey{
			ID: 83, UserID: 40, GroupID: &groupID, Key: "sk-zero-prompt-roundtrip", Status: StatusActive,
			User: &User{ID: 40, Status: StatusActive},
			Group: &Group{
				ID: groupID, Name: "zero-prompt", Platform: PlatformOpenAI, Status: StatusActive,
				Hydrated: true, OpenAIZeroPromptInjection: enabled,
			},
		}
		svc := &APIKeyService{}
		payload, err := json.Marshal(&APIKeyAuthCacheEntry{Snapshot: svc.snapshotFromAPIKey(context.Background(), apiKey)})
		require.NoError(t, err)
		var cached APIKeyAuthCacheEntry
		require.NoError(t, json.Unmarshal(payload, &cached))
		materialized, used, err := svc.applyAuthCacheEntry(apiKey.Key, &cached)
		require.NoError(t, err)
		require.True(t, used)
		require.True(t, materialized.Group.Hydrated)
		require.Equal(t, enabled, materialized.Group.OpenAIZeroPromptInjection)
	}
}

func TestAPIKeyAuthSnapshotRejectsVersionWithoutZeroPromptInjection(t *testing.T) {
	svc := &APIKeyService{}
	apiKey, used, err := svc.applyAuthCacheEntry("sk-legacy-prompt-policy", &APIKeyAuthCacheEntry{
		Snapshot: &APIKeyAuthSnapshot{Version: 28},
	})
	require.NoError(t, err)
	require.False(t, used, "refetch snapshots written by older instances rather than silently enabling prompt injection")
	require.Nil(t, apiKey)
}
