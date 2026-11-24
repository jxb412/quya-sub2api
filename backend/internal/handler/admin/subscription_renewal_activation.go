package admin

import (
	"strconv"

	"github.com/Wei-Shaw/sub2api/internal/pkg/adminaccess"
	"github.com/Wei-Shaw/sub2api/internal/pkg/response"
	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/gin-gonic/gin"
)

// 2026-11-24: only the trusted store/full administrator may attest payment.
// Ordinary users use the store's authenticated preview/confirmation flow.
func (h *SubscriptionHandler) ActivateRenewal(c *gin.Context) {
	if adminaccess.RestrictedFromContext(c.Request.Context()) != nil {
		response.Forbidden(c, "Full administrator access required")
		return
	}
	id, err := strconv.ParseInt(c.Param("id"), 10, 64)
	if err != nil || id <= 0 {
		response.BadRequest(c, "Invalid subscription ID")
		return
	}
	var input service.ActivateSubscriptionRenewalInput
	if err := c.ShouldBindJSON(&input); err != nil {
		response.BadRequest(c, "Invalid renewal request")
		return
	}
	receipt, err := h.subscriptionService.ActivateSubscriptionRenewal(c.Request.Context(), id, &input)
	if err != nil {
		response.ErrorFrom(c, err)
		return
	}
	response.Success(c, receipt)
}

func (h *SubscriptionHandler) GetRenewalActivation(c *gin.Context) {
	if adminaccess.RestrictedFromContext(c.Request.Context()) != nil {
		response.Forbidden(c, "Full administrator access required")
		return
	}
	id, err := strconv.ParseInt(c.Param("id"), 10, 64)
	if err != nil || id <= 0 {
		response.BadRequest(c, "Invalid subscription ID")
		return
	}
	receipt, err := h.subscriptionService.GetRenewalActivation(c.Request.Context(), id)
	if err != nil {
		response.ErrorFrom(c, err)
		return
	}
	response.Success(c, gin.H{"activated": receipt != nil, "receipt": receipt})
}
