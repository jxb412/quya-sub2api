package handler

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/server/middleware"
	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/gin-gonic/gin"
	"github.com/stretchr/testify/require"
)

func TestNativeSubscriptionResetHandlerRequiresIdentityAndStrictBody(t *testing.T) {
	gin.SetMode(gin.TestMode)
	for _, tc := range []struct {
		name, body    string
		authenticated bool
		want          int
	}{
		{"anonymous", `{}`, false, http.StatusUnauthorized},
		{"spoofed owner", `{"preview_token":"` + strings.Repeat("a", 64) + `","accepted_rules":true,"user_id":10}`, true, http.StatusBadRequest},
		{"spoofed codes", `{"preview_token":"` + strings.Repeat("a", 64) + `","accepted_rules":true,"pending_codes":["store_forged"]}`, true, http.StatusBadRequest},
		{"rules not accepted", `{"preview_token":"` + strings.Repeat("a", 64) + `","accepted_rules":false}`, true, http.StatusBadRequest},
		{"two bodies", `{} {}`, true, http.StatusBadRequest},
	} {
		t.Run(tc.name, func(t *testing.T) {
			r := gin.New()
			h := NewSubscriptionHandler(&service.SubscriptionService{})
			if tc.authenticated {
				r.Use(func(c *gin.Context) { c.Set(string(middleware.ContextKeyUser), middleware.AuthSubject{UserID: 10}) })
			}
			r.POST("/subscriptions/:id/reset", h.Reset)
			req := httptest.NewRequest(http.MethodPost, "/subscriptions/123/reset", strings.NewReader(tc.body))
			req.Header.Set("Content-Type", "application/json")
			rec := httptest.NewRecorder()
			r.ServeHTTP(rec, req)
			require.Equal(t, tc.want, rec.Code, rec.Body.String())
			require.Equal(t, "no-store", rec.Header().Get("Cache-Control"))
		})
	}
}
