package service

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"regexp"
	"strings"
	"time"

	dbent "github.com/Wei-Shaw/sub2api/ent"
	infraerrors "github.com/Wei-Shaw/sub2api/internal/pkg/errors"
	"github.com/Wei-Shaw/sub2api/internal/pkg/timezone"
	"github.com/lib/pq"
)

// 2026-11-24 Quya: native user and legacy admin flows validate local redeemed
// store rights and replace the old period atomically. No PHP payment API is used.
// pending_codes excludes next_code: it contains only the later queued orders.
type ActivateSubscriptionRenewalInput struct {
	UserID            int64     `json:"user_id"`
	GroupID           int64     `json:"group_id"`
	CurrentCode       string    `json:"current_code"`
	NextCode          string    `json:"next_code"`
	PendingCodes      []string  `json:"pending_codes"`
	ExpectedStartsAt  time.Time `json:"expected_starts_at"`
	ExpectedExpiresAt time.Time `json:"expected_expires_at"`
	ExpectedNotes     string    `json:"expected_notes"`
	IdempotencyKey    string    `json:"idempotency_key"`
}

type SubscriptionRenewalActivationReceipt struct {
	OldSubscriptionID   int64     `json:"old_subscription_id"`
	NewSubscriptionID   int64     `json:"new_subscription_id"`
	UserID              int64     `json:"user_id"`
	GroupID             int64     `json:"group_id"`
	CurrentCode         string    `json:"current_code"`
	PreviousCode        string    `json:"previous_code,omitempty"`
	PendingCodes        []string  `json:"pending_codes"`
	CurrentValidityDays int       `json:"current_validity_days"`
	TotalValidityDays   int       `json:"total_validity_days"`
	StartsAt            time.Time `json:"starts_at"`
	ExpiresAt           time.Time `json:"expires_at"`
	IdempotencyKey      string    `json:"idempotency_key"`
	Replayed            bool      `json:"replayed"`
}

type renewalSQLClient interface {
	QueryContext(context.Context, string, ...any) (*sql.Rows, error)
	ExecContext(context.Context, string, ...any) (sql.Result, error)
}

var renewalCodePattern = regexp.MustCompile(`^store_[A-Za-z0-9_-]{1,26}$`)
var renewalKeyPattern = regexp.MustCompile(`^[A-Za-z0-9:_-]{16,160}$`)

func renewalConflict(message string) error {
	return infraerrors.Conflict("SUBSCRIPTION_RENEWAL_CONFLICT", message)
}

func renewalPeriod(now time.Time, days int) (time.Time, time.Time) {
	// Match the store's purchased 86400-second days across DST changes and the
	// database's microsecond precision so durable receipts equal stored values.
	start := now.UTC().Truncate(time.Microsecond)
	return start, start.Add(time.Duration(days) * 24 * time.Hour)
}

func validateRenewalActivationInput(id int64, in *ActivateSubscriptionRenewalInput) error {
	if in == nil || id <= 0 || in.UserID <= 0 || in.GroupID <= 0 ||
		in.ExpectedStartsAt.IsZero() || in.ExpectedExpiresAt.IsZero() ||
		len(in.ExpectedNotes) > 65536 || len(in.PendingCodes) > 99 || !renewalKeyPattern.MatchString(in.IdempotencyKey) {
		return infraerrors.BadRequest("INVALID_RENEWAL_ACTIVATION", "invalid subscription renewal request")
	}
	seen := make(map[string]bool)
	for _, code := range append([]string{in.CurrentCode, in.NextCode}, in.PendingCodes...) {
		if !renewalCodePattern.MatchString(code) || seen[code] {
			return infraerrors.BadRequest("INVALID_RENEWAL_ACTIVATION", "purchase codes must be valid and distinct")
		}
		seen[code] = true
	}
	return nil
}

func renewalActivationHash(id int64, in *ActivateSubscriptionRenewalInput) string {
	copy := *in
	copy.ExpectedStartsAt = copy.ExpectedStartsAt.UTC()
	copy.ExpectedExpiresAt = copy.ExpectedExpiresAt.UTC()
	if copy.PendingCodes == nil {
		copy.PendingCodes = []string{}
	}
	raw, _ := json.Marshal(struct {
		ID    int64
		Input ActivateSubscriptionRenewalInput
	}{id, copy})
	sum := sha256.Sum256(raw)
	return hex.EncodeToString(sum[:])
}

func readRenewalActivation(ctx context.Context, client renewalSQLClient, id int64) (*SubscriptionRenewalActivationReceipt, string, error) {
	rows, err := client.QueryContext(ctx, `SELECT receipt, request_hash FROM subscription_renewal_activations WHERE old_subscription_id=$1`, id)
	if err != nil {
		return nil, "", err
	}
	defer rows.Close()
	if !rows.Next() {
		return nil, "", rows.Err()
	}
	var raw []byte
	var hash string
	if err := rows.Scan(&raw, &hash); err != nil {
		return nil, "", err
	}
	var receipt SubscriptionRenewalActivationReceipt
	if err := json.Unmarshal(raw, &receipt); err != nil {
		return nil, "", err
	}
	return &receipt, hash, nil
}

// GetRenewalActivation remains available for soft-deleted old subscriptions so
// store refund recovery can distinguish this replacement from its own revoke.
func (s *SubscriptionService) GetRenewalActivation(ctx context.Context, id int64) (*SubscriptionRenewalActivationReceipt, error) {
	if s.entClient == nil {
		return nil, fmt.Errorf("renewal activation requires database access")
	}
	r, _, err := readRenewalActivation(ctx, s.entClient, id)
	return r, err
}

func (s *SubscriptionService) revokeSubscriptionBeforeRenewal(ctx context.Context, id int64) error {
	var sub *UserSubscription
	err := s.withSubscriptionUpdateTx(ctx, func(txCtx context.Context) error {
		var err error
		if err = lockStoreSubscriptionUserForID(txCtx, dbent.TxFromContext(txCtx).Client(), id); err != nil {
			return err
		}
		sub, err = s.userSubRepo.GetByIDForUpdate(txCtx, id)
		if err != nil {
			return err
		}
		receipt, _, err := readRenewalActivation(txCtx, dbent.TxFromContext(txCtx).Client(), id)
		if err != nil {
			return err
		}
		if receipt != nil {
			return renewalConflict("subscription was already consumed by renewal activation")
		}
		return s.userSubRepo.Delete(txCtx, id)
	})
	if err != nil {
		return err
	}
	// Bulk operations own the outer transaction and invalidate after commit.
	if dbent.TxFromContext(ctx) != nil {
		return nil
	}
	return s.invalidateSubscriptionCaches(sub.UserID, sub.GroupID)
}

func (s *SubscriptionService) ActivateSubscriptionRenewal(ctx context.Context, id int64, in *ActivateSubscriptionRenewalInput) (*SubscriptionRenewalActivationReceipt, error) {
	if err := validateRenewalActivationInput(id, in); err != nil {
		return nil, err
	}
	if s.entClient == nil {
		return nil, fmt.Errorf("renewal activation requires database access")
	}
	if tx := dbent.TxFromContext(ctx); tx != nil {
		if err := lockStoreSubscriptionUser(ctx, tx.Client(), in.UserID); err != nil {
			return nil, err
		}
		return s.activateSubscriptionRenewalLocked(ctx, tx.Client(), id, in)
	}
	tx, err := s.entClient.Tx(ctx)
	if err != nil {
		return nil, err
	}
	defer func() { _ = tx.Rollback() }()
	if err := lockStoreSubscriptionUser(ctx, tx.Client(), in.UserID); err != nil {
		return nil, err
	}
	receipt, err := s.activateSubscriptionRenewalLocked(ctx, tx.Client(), id, in)
	if err != nil {
		return nil, err
	}
	if err := tx.Commit(); err != nil {
		return nil, err
	}
	if err := s.invalidateSubscriptionCaches(in.UserID, in.GroupID); err != nil {
		return nil, err
	}
	return receipt, nil
}

func (s *SubscriptionService) activateSubscriptionRenewalLocked(ctx context.Context, client renewalSQLClient, id int64, in *ActivateSubscriptionRenewalInput) (*SubscriptionRenewalActivationReceipt, error) {
	// Include soft-deleted rows in this lock, so concurrent retries wait for the
	// first commit and then read its durable receipt instead of activating twice.
	rows, err := client.QueryContext(ctx, `SELECT user_id, group_id, status, starts_at, expires_at, COALESCE(notes,''), deleted_at FROM user_subscriptions WHERE id=$1 FOR UPDATE`, id)
	if err != nil {
		return nil, err
	}
	var sub UserSubscription
	var deleted sql.NullTime
	if !rows.Next() {
		rows.Close()
		return nil, ErrSubscriptionNotFound
	}
	err = rows.Scan(&sub.UserID, &sub.GroupID, &sub.Status, &sub.StartsAt, &sub.ExpiresAt, &sub.Notes, &deleted)
	rows.Close()
	if err != nil {
		return nil, err
	}
	if sub.UserID != in.UserID || sub.GroupID != in.GroupID {
		return nil, renewalConflict("subscription ownership changed")
	}
	hash := renewalActivationHash(id, in)
	prior, priorHash, err := readRenewalActivation(ctx, client, id)
	if err != nil {
		return nil, err
	}
	if prior != nil {
		if priorHash != hash || prior.IdempotencyKey != in.IdempotencyKey {
			return nil, renewalConflict("subscription already activated by another operation")
		}
		prior.Replayed = true
		return prior, nil
	}
	// 2026-11-24: prepaid renewal activation assumes Quya lifetime quota windows.
	// Automatic resets may already have granted a queued purchase naturally.
	if s.autoResetEnabled {
		return nil, renewalConflict("renewal activation requires automatic quota reset to be disabled; ask an administrator to verify the subscription rules")
	}
	now := time.Now()
	if s.now != nil {
		now = s.now()
	}
	now, _ = renewalPeriod(now, 0)
	if deleted.Valid || sub.Status != SubscriptionStatusActive || !sub.ExpiresAt.After(now.Add(31*24*time.Hour)) {
		return nil, renewalConflict("active subscription must have more than 31 days remaining")
	}
	if !sub.StartsAt.Equal(in.ExpectedStartsAt) || !sub.ExpiresAt.Equal(in.ExpectedExpiresAt) || sub.Notes != in.ExpectedNotes {
		return nil, renewalConflict("subscription changed; refresh the renewal preview")
	}
	sources, err := resolveNativeSubscriptionSources(ctx, client, id, in.UserID, in.GroupID)
	if err != nil {
		return nil, err
	}
	expected := sources.activationInput()
	if expected == nil || expected.CurrentCode != in.CurrentCode || expected.NextCode != in.NextCode || strings.Join(expected.PendingCodes, "\x00") != strings.Join(in.PendingCodes, "\x00") {
		return nil, renewalConflict("purchase queue changed; refresh the renewal preview")
	}
	rows, err = client.QueryContext(ctx, `SELECT id FROM groups WHERE id=$1 AND deleted_at IS NULL AND status='active' AND subscription_type='subscription' FOR SHARE`, in.GroupID)
	if err != nil {
		return nil, err
	}
	groupOK := rows.Next()
	rows.Close()
	if !groupOK {
		return nil, ErrGroupNotSubscriptionType
	}
	codes := append([]string{in.CurrentCode, in.NextCode}, in.PendingCodes...)
	rows, err = client.QueryContext(ctx, `SELECT code, type, status, used_by, group_id, validity_days, used_at FROM redeem_codes WHERE code=ANY($1) ORDER BY id FOR SHARE`, pq.Array(codes))
	if err != nil {
		return nil, err
	}
	daysByCode := make(map[string]int)
	for rows.Next() {
		var code, typ, status string
		var user, group sql.NullInt64
		var days int
		var used sql.NullTime
		if err := rows.Scan(&code, &typ, &status, &user, &group, &days, &used); err != nil {
			rows.Close()
			return nil, err
		}
		if typ != "subscription" || status != "used" || !used.Valid || !user.Valid || user.Int64 != in.UserID || !group.Valid || group.Int64 != in.GroupID || days < 1 || days > MaxValidityDays {
			rows.Close()
			return nil, renewalConflict("purchase code does not match this subscription")
		}
		if code != in.CurrentCode && days != 30 && days != 31 {
			rows.Close()
			return nil, renewalConflict("renewal must be a 30 or 31 day purchase")
		}
		daysByCode[code] = days
	}
	err = rows.Err()
	rows.Close()
	if err != nil {
		return nil, err
	}
	if len(daysByCode) != len(codes) {
		return nil, renewalConflict("purchase code history is incomplete")
	}
	queued := append([]string{in.NextCode}, in.PendingCodes...)
	rows, err = client.QueryContext(ctx, `SELECT old_subscription_id FROM subscription_renewal_activations WHERE next_code=ANY($1) LIMIT 1`, pq.Array(queued))
	if err != nil {
		return nil, err
	}
	alreadyUsed := rows.Next()
	rows.Close()
	if alreadyUsed {
		return nil, renewalConflict("a queued purchase has already been activated")
	}
	totalDays := 0
	for _, code := range queued {
		totalDays += daysByCode[code]
	}
	_, expires := renewalPeriod(now, totalDays)
	if expires.After(MaxExpiresAt) {
		return nil, renewalConflict("renewal expiry exceeds supported range")
	}
	// Keep the exact existing store lineage format; refunds use these markers to
	// distinguish the newly active order from later unconsumed queued purchases.
	notes := "通过兑换码 " + in.NextCode + " 兑换\n套餐当前购买码: " + in.NextCode + "\n套餐待启用购买码: " + strings.Join(in.PendingCodes, " ") + fmt.Sprintf("\n套餐循环检查自动重开，原订阅ID %d", id)
	if _, err := client.ExecContext(ctx, `UPDATE user_subscriptions SET deleted_at=$2, updated_at=$2 WHERE id=$1 AND deleted_at IS NULL`, id, now); err != nil {
		return nil, err
	}
	rows, err = client.QueryContext(ctx, `INSERT INTO user_subscriptions (user_id,group_id,starts_at,expires_at,status,daily_window_start,weekly_window_start,monthly_window_start,daily_usage_usd,weekly_usage_usd,monthly_usage_usd,assigned_at,notes,created_at,updated_at) VALUES ($1,$2,$3,$4,'active',$5,$3,$3,0,0,0,$3,$6,$3,$3) RETURNING id`, in.UserID, in.GroupID, now, expires, timezone.StartOfDay(now), notes)
	if err != nil {
		return nil, err
	}
	var newID int64
	if !rows.Next() {
		rows.Close()
		return nil, fmt.Errorf("renewal insert returned no subscription")
	}
	err = rows.Scan(&newID)
	rows.Close()
	if err != nil {
		return nil, err
	}
	pending := append([]string{}, in.PendingCodes...)
	receipt := &SubscriptionRenewalActivationReceipt{OldSubscriptionID: id, NewSubscriptionID: newID, UserID: in.UserID, GroupID: in.GroupID, PreviousCode: in.CurrentCode, CurrentCode: in.NextCode, PendingCodes: pending, CurrentValidityDays: daysByCode[in.NextCode], TotalValidityDays: totalDays, StartsAt: now, ExpiresAt: expires, IdempotencyKey: in.IdempotencyKey}
	raw, _ := json.Marshal(receipt)
	_, err = client.ExecContext(ctx, `INSERT INTO subscription_renewal_activations (old_subscription_id,new_subscription_id,user_id,group_id,next_code,idempotency_key,request_hash,receipt,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)`, id, newID, in.UserID, in.GroupID, in.NextCode, in.IdempotencyKey, hash, string(raw), now)
	if err != nil {
		return nil, err
	}
	return receipt, nil
}
