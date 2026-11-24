package service

import (
	"net/url"
	"strings"
	"unicode"

	infraerrors "github.com/Wei-Shaw/sub2api/internal/pkg/errors"
)

// 2026-11-24: this administrator-selected storefront receives the signed-in
// member's identity for purchase verification. Keep it disabled by default and
// require an unambiguous HTTPS URL without credentials or a fragment.
func ValidateSubscriptionResetURL(raw string) error {
	raw = strings.TrimSpace(raw)
	if raw == "" {
		return nil
	}
	invalid := func() error {
		return infraerrors.BadRequest("INVALID_SUBSCRIPTION_RESET_URL", "Subscription reset URL must be an absolute HTTPS URL without user information or a fragment")
	}
	if len(raw) > 2048 || strings.Contains(raw, "\\") || strings.IndexFunc(raw, unicode.IsControl) >= 0 {
		return invalid()
	}
	u, err := url.Parse(raw)
	if err != nil || u.Scheme != "https" || u.Hostname() == "" || u.User != nil || u.Fragment != "" || u.Opaque != "" {
		return invalid()
	}
	return nil
}

func publicSubscriptionResetURL(raw string) string {
	if ValidateSubscriptionResetURL(raw) != nil {
		return ""
	}
	return strings.TrimSpace(raw)
}
