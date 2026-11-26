//go:build unit

package service

import (
	"context"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestAdminGroupZeroPromptInjectionCreateAndCopy(t *testing.T) {
	for _, tc := range []struct {
		name     string
		platform string
		enabled  bool
		want     bool
	}{
		{"default remains off", PlatformOpenAI, false, false},
		{"openai opt in", PlatformOpenAI, true, true},
		{"anthropic ignores policy", PlatformAnthropic, true, false},
		{"composite ignores policy", PlatformComposite, true, false},
	} {
		t.Run(tc.name, func(t *testing.T) {
			repo := &groupRepoStubForAdmin{}
			svc := &adminServiceImpl{groupRepo: repo}
			created, err := svc.CreateGroup(context.Background(), &CreateGroupInput{
				Name: "zero-prompt", Platform: tc.platform, RateMultiplier: 1,
				OpenAIZeroPromptInjection: tc.enabled,
			})
			require.NoError(t, err)
			require.Equal(t, tc.want, created.OpenAIZeroPromptInjection)
			require.Equal(t, tc.want, repo.created.OpenAIZeroPromptInjection)
			copied := cloneGroupForDuplicate(created, "zero-prompt-copy")
			require.Equal(t, tc.want, copied.OpenAIZeroPromptInjection)
		})
	}
}

func TestAdminGroupZeroPromptInjectionUpdate(t *testing.T) {
	truth, falsity := true, false
	for _, tc := range []struct {
		name     string
		existing bool
		input    UpdateGroupInput
		want     bool
	}{
		{"enable", false, UpdateGroupInput{OpenAIZeroPromptInjection: &truth}, true},
		{"explicit disable", true, UpdateGroupInput{OpenAIZeroPromptInjection: &falsity}, false},
		{"omission preserves", true, UpdateGroupInput{Name: "renamed"}, true},
		{"platform change clears", true, UpdateGroupInput{Platform: PlatformAnthropic}, false},
		{"cannot enable on other platform", false, UpdateGroupInput{Platform: PlatformComposite, OpenAIZeroPromptInjection: &truth}, false},
	} {
		t.Run(tc.name, func(t *testing.T) {
			repo := &groupRepoStubForAdmin{getByID: &Group{
				ID: 7, Name: "zero-prompt", Platform: PlatformOpenAI, Status: StatusActive,
				OpenAIZeroPromptInjection: tc.existing,
			}}
			invalidator := &authCacheInvalidatorStub{}
			svc := &adminServiceImpl{groupRepo: repo, authCacheInvalidator: invalidator}
			updated, err := svc.UpdateGroup(context.Background(), 7, &tc.input)
			require.NoError(t, err)
			require.Equal(t, tc.want, updated.OpenAIZeroPromptInjection)
			require.Equal(t, tc.want, repo.updated.OpenAIZeroPromptInjection)
			require.Equal(t, []int64{7}, invalidator.groupIDs, "policy changes must reach existing API keys")
		})
	}
}
