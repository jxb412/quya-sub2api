package routes

import (
	"github.com/Wei-Shaw/sub2api/internal/pkg/response"
	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/gin-gonic/gin"
)

// 2026-11-24: hiding the button is insufficient; stop preview and mutation on
// the server until every instance supports renewal receipts and period caches.
func subscriptionResetEnabledGuard(settings *service.SettingService) gin.HandlerFunc {
	return func(c *gin.Context) {
		c.Header("Cache-Control", "no-store")
		if !settings.IsSubscriptionResetEnabled(c.Request.Context()) {
			response.Forbidden(c, "订阅自助重置暂未开放")
			c.Abort()
			return
		}
		c.Next()
	}
}
