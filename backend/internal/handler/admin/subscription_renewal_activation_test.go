package admin

import (
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/pkg/adminaccess"
	"github.com/gin-gonic/gin"
	"github.com/stretchr/testify/require"
)

func TestRenewalHandlerRejectsRestrictedAdminBeforeDatabaseAccess(t *testing.T) {
	gin.SetMode(gin.TestMode)
	h := NewSubscriptionHandler(nil)
	for _, method := range []string{"POST", "GET"} {
		w := httptest.NewRecorder()
		c, _ := gin.CreateTestContext(w)
		c.Request = httptest.NewRequest(method, "/subscriptions/1/activate-renewal", strings.NewReader(`{}`))
		c.Request = c.Request.WithContext(adminaccess.WithPrincipal(c.Request.Context(), &adminaccess.Principal{Policy: adminaccess.Policy{GlobalPermissions: []string{"subscriptions.read", "subscriptions.reset", "subscriptions.extend"}}}))
		c.Params = gin.Params{{Key: "id", Value: "1"}}
		if method == "GET" {
			h.GetRenewalActivation(c)
		} else {
			h.ActivateRenewal(c)
		}
		require.Equal(t, 403, w.Code)
	}
}

func TestRenewalHandlerRejectsMalformedPreview(t *testing.T) {
	gin.SetMode(gin.TestMode)
	h := NewSubscriptionHandler(nil)
	for _, tc := range []struct{ id, body string }{{"-1", `{}`}, {"not-a-number", `{}`}, {"1", `{"expected_starts_at":"yesterday"}`}} {
		w := httptest.NewRecorder()
		c, _ := gin.CreateTestContext(w)
		c.Request = httptest.NewRequest("POST", "/subscriptions/"+tc.id+"/activate-renewal", strings.NewReader(tc.body))
		c.Request.Header.Set("Content-Type", "application/json")
		c.Params = gin.Params{{Key: "id", Value: tc.id}}
		h.ActivateRenewal(c)
		require.Equal(t, 400, w.Code)
	}
}
