package admin

import (
	"strconv"

	"github.com/Wei-Shaw/sub2api/internal/handler/dto"
	"github.com/Wei-Shaw/sub2api/internal/pkg/response"
	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/gin-gonic/gin"
)

// 2026-11-26: one administrator store call replaces revoke + assign. The
// service validates purchase lineage and owns a durable transaction receipt.
func (h *SubscriptionHandler) ReplaceStore(c *gin.Context) {
	id, err := strconv.ParseInt(c.Param("id"), 10, 64)
	if err != nil || id <= 0 {
		response.BadRequest(c, "Invalid subscription ID")
		return
	}
	var req service.ReplaceStoreSubscriptionInput
	if err := c.ShouldBindJSON(&req); err != nil {
		response.BadRequest(c, "Invalid atomic store replacement request")
		return
	}
	req.AssignedBy = getAdminIDFromContext(c)
	result, err := h.subscriptionService.ReplaceStoreSubscription(c.Request.Context(), id, &req)
	if err != nil {
		response.ErrorFrom(c, err)
		return
	}
	response.Success(c, gin.H{
		"old_subscription_id": result.OldSubscriptionID,
		"subscription":        adminSubscriptionResponse(c, dto.UserSubscriptionFromServiceAdmin(result.Subscription)),
		"replayed":            result.Replayed,
	})
}
