package adminaccess

import (
	"net/http"
	"testing"

	"github.com/stretchr/testify/require"
)

// 2026-11-26: the atomic store operation must not expand existing scoped keys.
func TestStoreSubscriptionReplaceRequiresExplicitGlobalPermission(t *testing.T) {
	const path = "/api/v1/admin/subscriptions/:id/replace-store"
	for _, permissions := range [][]string{
		nil,
		{"subscriptions.read"},
		{"subscriptions.assign"},
		{"subscriptions.revoke"},
		{"subscriptions.reset"},
		{"subscriptions.assign", "subscriptions.revoke"},
	} {
		policy := Policy{GlobalPermissions: permissions}
		require.False(t, RouteAllowed(policy, http.MethodPost, path), "existing grants must not authorize replacement: %v", permissions)
	}

	policy := Policy{GlobalPermissions: []string{"subscriptions.replace"}}
	require.NoError(t, ValidatePolicy(policy))
	require.True(t, RouteAllowed(policy, http.MethodPost, path))
	for _, method := range []string{http.MethodGet, http.MethodPut, http.MethodDelete} {
		require.False(t, RouteAllowed(policy, method, path))
	}
	for _, other := range []string{
		"/api/v1/admin/subscriptions/assign",
		"/api/v1/admin/subscriptions/:id/revoke",
		"/api/v1/admin/subscriptions/:id/restore",
		"/api/v1/admin/subscriptions/:id/reset-quota",
		"/api/v1/admin/subscriptions/:id/activate-renewal",
	} {
		require.False(t, RouteAllowed(policy, http.MethodPost, other), "replacement must not grant other subscription actions: %s", other)
	}
	require.True(t, RouteAllowed(Policy{FullAccess: true}, http.MethodPost, path))
}

func TestStoreSubscriptionReplaceCannotBeGrantedAsAccountRule(t *testing.T) {
	policy := Policy{Rules: []Rule{{Permissions: []string{"subscriptions.replace"}, GroupIDs: []int64{1}, AccountTypes: []string{"oauth"}}}}
	require.Error(t, ValidatePolicy(policy))
	require.False(t, RouteAllowed(policy, http.MethodPost, "/api/v1/admin/subscriptions/:id/replace-store"))
}
