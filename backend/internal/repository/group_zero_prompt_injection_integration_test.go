//go:build integration

package repository

import (
	"context"
	"fmt"
	"testing"
	"time"

	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/stretchr/testify/require"
)

func TestGroupZeroPromptInjectionPersistsAndReachesAuthProjection(t *testing.T) {
	ctx := context.Background()
	tx := testEntTx(t)
	groupRepo := newGroupRepositoryWithSQL(tx.Client(), tx)
	group := &service.Group{
		Name: fmt.Sprintf("zero-prompt-%d", time.Now().UnixNano()), Platform: service.PlatformOpenAI,
		RateMultiplier: 1, Status: service.StatusActive, SubscriptionType: service.SubscriptionTypeStandard,
		OpenAIZeroPromptInjection: true,
	}
	require.NoError(t, groupRepo.Create(ctx, group))
	loaded, err := groupRepo.GetByIDLite(ctx, group.ID)
	require.NoError(t, err)
	require.True(t, loaded.OpenAIZeroPromptInjection)

	user := mustCreateUser(t, tx.Client(), &service.User{Email: group.Name + "@example.com", Concurrency: 5})
	keyRepo := newAPIKeyRepositoryWithSQL(tx.Client(), tx)
	key := &service.APIKey{
		UserID: user.ID, GroupID: &group.ID, Key: "sk-" + group.Name, Name: "zero-prompt", Status: service.StatusActive,
	}
	require.NoError(t, keyRepo.Create(ctx, key))
	auth, err := keyRepo.GetByKeyForAuth(ctx, key.Key)
	require.NoError(t, err)
	require.NotNil(t, auth.Group)
	require.True(t, auth.Group.OpenAIZeroPromptInjection, "lightweight auth SELECT must include this group policy")

	group.OpenAIZeroPromptInjection = false
	require.NoError(t, groupRepo.Update(ctx, group))
	auth, err = keyRepo.GetByKeyForAuth(ctx, key.Key)
	require.NoError(t, err)
	require.False(t, auth.Group.OpenAIZeroPromptInjection)
}
