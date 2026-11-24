package handler

import (
	"encoding/json"
	"io"
	"net/http"
	"strconv"

	"github.com/Wei-Shaw/sub2api/internal/pkg/response"
	middleware2 "github.com/Wei-Shaw/sub2api/internal/server/middleware"
	"github.com/gin-gonic/gin"
)

// 2026-11-24: identity comes only from the authenticated panel session. Reject
// arbitrary code/user/group fields rather than silently accepting authority hints.
func (h *SubscriptionHandler) ResetPreview(c *gin.Context) {
	c.Header("Cache-Control", "no-store")
	subject, ok := middleware2.GetAuthSubjectFromContext(c)
	if !ok {
		response.Unauthorized(c, "User not found in context")
		return
	}
	id, err := strconv.ParseInt(c.Param("id"), 10, 64)
	if err != nil || id <= 0 {
		response.BadRequest(c, "Invalid subscription ID")
		return
	}
	preview, err := h.subscriptionService.PreviewSubscriptionReset(c.Request.Context(), subject.UserID, id)
	if err != nil {
		response.ErrorFrom(c, err)
		return
	}
	response.Success(c, preview)
}

func (h *SubscriptionHandler) Reset(c *gin.Context) {
	c.Header("Cache-Control", "no-store")
	subject, ok := middleware2.GetAuthSubjectFromContext(c)
	if !ok {
		response.Unauthorized(c, "User not found in context")
		return
	}
	id, err := strconv.ParseInt(c.Param("id"), 10, 64)
	if err != nil || id <= 0 {
		response.BadRequest(c, "Invalid subscription ID")
		return
	}
	var input struct {
		PreviewToken  string `json:"preview_token"`
		AcceptedRules bool   `json:"accepted_rules"`
	}
	decoder := json.NewDecoder(http.MaxBytesReader(c.Writer, c.Request.Body, 4096))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&input); err != nil {
		response.BadRequest(c, "Invalid subscription reset request")
		return
	}
	var extra any
	if err := decoder.Decode(&extra); err != io.EOF {
		response.BadRequest(c, "Invalid subscription reset request")
		return
	}
	result, err := h.subscriptionService.ResetUserSubscription(c.Request.Context(), subject.UserID, id, input.PreviewToken, input.AcceptedRules)
	if err != nil {
		response.ErrorFrom(c, err)
		return
	}
	response.Success(c, result)
}
