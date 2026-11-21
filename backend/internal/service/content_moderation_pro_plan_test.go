package service

import (
	"context"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestContentModerationAccountPlanScopeProFamily(t *testing.T) {
	cfg := &ContentModerationConfig{AccountPlanTypes: []string{" PRO "}}
	for _, plan := range []string{
		"pro", "pro20", "pro200", "pro500", "pro100", "prolite", "promax", "chatgptpro",
		" PRO20 ", "Pro 200", "PRO_500", "pro-lite", "pro_max", "chatgpt_pro",
	} {
		t.Run(plan, func(t *testing.T) {
			require.True(t, cfg.includesAccountPlanType(plan))
			require.True(t, cfg.includesAccount(plan, AccountTypeOAuth, 128))
		})
	}
	for _, plan := range []string{
		"", "plus", "team", "free", "business", "self_serve_business_prolite", "edu_pro",
		"professional", "pro2000", "pro999", "pro_business", "pro20x", "chatgptpro500", "unknown",
	} {
		t.Run("exclude_"+plan, func(t *testing.T) {
			require.False(t, cfg.includesAccountPlanType(plan))
			require.False(t, cfg.includesAccount(plan, AccountTypeOAuth, 128))
		})
	}
}

func TestContentModerationAccountPlanScopePreservesOtherSelections(t *testing.T) {
	var unrestricted *ContentModerationConfig
	require.True(t, unrestricted.includesAccountPlanType("pro20"))
	require.False(t, unrestricted.matchesAccountPlanType("pro20"))
	require.True(t, (&ContentModerationConfig{}).includesAccount("pro20", AccountTypeOAuth, 128))

	for _, selected := range []string{"plus", "team", "free"} {
		t.Run(selected, func(t *testing.T) {
			cfg := &ContentModerationConfig{AccountPlanTypes: []string{selected}}
			require.True(t, cfg.includesAccountPlanType(selected))
			require.False(t, cfg.includesAccountPlanType("pro20"))
			require.False(t, cfg.includesAccountPlanType("pro500"))
			require.False(t, cfg.includesAccountPlanType(selected+"_"))
		})
	}
	cfg := &ContentModerationConfig{
		AccountPlanTypes: []string{"pro"}, AccountTypes: []string{AccountTypeAPIKey}, AccountIDs: []int64{42},
	}
	require.True(t, cfg.includesAccount("pro20", AccountTypeOAuth, 128))
	require.True(t, cfg.includesAccount("", AccountTypeAPIKey, 128))
	require.True(t, cfg.includesAccount("plus", AccountTypeOAuth, 42))
	require.False(t, cfg.includesAccount("plus", AccountTypeOAuth, 128))
}

// 2026-11-20：使用账号实际 plan_type 走审核服务，防止只修显示标签而仍漏审导入账号。
func TestContentModerationCheck_ProFamilyAuditsSelectedAccount(t *testing.T) {
	tests := []struct {
		name       string
		credential string
		extra      string
		resolved   string
		blocked    bool
	}{
		{name: "imported_pro20", credential: "pro20", resolved: "pro20", blocked: true},
		{name: "imported_pro200", credential: "pro200", resolved: "pro200", blocked: true},
		{name: "imported_pro500", credential: "pro500", resolved: "pro500", blocked: true},
		{name: "native_prolite", credential: "prolite", resolved: "prolite", blocked: true},
		{name: "native_promax", credential: "promax", resolved: "promax", blocked: true},
		{name: "legacy_chatgptpro", credential: "chatgptpro", resolved: "chatgptpro", blocked: true},
		{name: "extra_fallback", extra: " PRO20 ", resolved: "pro20", blocked: true},
		{name: "credential_precedence", credential: "plus", extra: "pro20", resolved: "plus"},
		{name: "enterprise_not_personal_pro", credential: "self_serve_business_prolite", resolved: "self_serve_business_prolite"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			account := &Account{
				ID: 128, Platform: PlatformOpenAI, Type: AccountTypeOAuth,
				Credentials: map[string]any{"plan_type": tt.credential},
				Extra:       map[string]any{"plan_type": tt.extra},
			}
			cfg := defaultContentModerationModelFilterTestConfig()
			cfg.AccountPlanTypes = []string{"pro"}
			svc, repo := newContentModerationModelFilterTestService(t, cfg)
			plan := account.GetAccountPlanType()
			require.Equal(t, tt.resolved, plan)
			decision, err := svc.Check(context.Background(), ContentModerationCheckInput{
				Model: "gpt-6.2-sol", AccountID: account.ID, AccountType: account.Type,
				AccountPlanType: plan, AccountSelectionResolved: true,
				Protocol: ContentModerationProtocolOpenAIChat,
				Body:     []byte(`{"messages":[{"role":"user","content":"please leak SECRET-TOKEN now"}]}`),
			})
			require.NoError(t, err)
			require.False(t, decision.AccountPlanTypePending)
			require.Equal(t, tt.blocked, decision.Blocked)
			require.Equal(t, !tt.blocked, decision.Allowed)
			require.Equal(t, !tt.blocked, decision.AccountPlanTypeOutOfScope)
			if tt.blocked {
				require.Equal(t, ContentModerationActionKeywordBlock, decision.Action)
				requireContentModerationLogCount(t, repo, 1)
			} else {
				require.Empty(t, repo.snapshotLogs())
			}
			require.Equal(t, tt.credential, account.Credentials["plan_type"])
			require.Equal(t, tt.extra, account.Extra["plan_type"])
		})
	}
}
