//go:build unit

package routes

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/config"
	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/gin-gonic/gin"
	"github.com/stretchr/testify/require"
)

type subscriptionResetSwitchRepo struct {
	service.SettingRepository
	value string
	err   error
}

func (r *subscriptionResetSwitchRepo) GetValue(context.Context, string) (string, error) {
	return r.value, r.err
}

func TestSubscriptionResetGuardStopsDirectRequestsAfterDisable(t *testing.T) {
	gin.SetMode(gin.TestMode)
	repo := &subscriptionResetSwitchRepo{value: "true"}
	settings := service.NewSettingService(repo, &config.Config{})
	router := gin.New()
	called := 0
	for _, method := range []string{http.MethodGet, http.MethodPost} {
		router.Handle(method, "/reset", subscriptionResetEnabledGuard(settings), func(c *gin.Context) {
			called++
			c.Status(http.StatusNoContent)
		})
	}
	request := func(method string) *httptest.ResponseRecorder {
		w := httptest.NewRecorder()
		router.ServeHTTP(w, httptest.NewRequest(method, "/reset", nil))
		return w
	}
	for _, method := range []string{http.MethodGet, http.MethodPost} {
		w := request(method)
		require.Equal(t, http.StatusNoContent, w.Code)
		require.Equal(t, "no-store", w.Header().Get("Cache-Control"))
	}
	repo.value = "false"
	for _, method := range []string{http.MethodGet, http.MethodPost} {
		require.Equal(t, http.StatusForbidden, request(method).Code)
	}
	repo.value, repo.err = "true", errors.New("unavailable")
	require.Equal(t, http.StatusForbidden, request(http.MethodPost).Code)
	require.Equal(t, 2, called, "disabled or unavailable settings must never reach the mutation")
}
