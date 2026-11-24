package repository

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"fmt"
	"sync"
	"testing"
	"time"

	"entgo.io/ent/dialect"
	entsql "entgo.io/ent/dialect/sql"
	dbent "github.com/Wei-Shaw/sub2api/ent"
	infraerrors "github.com/Wei-Shaw/sub2api/internal/pkg/errors"
	"github.com/Wei-Shaw/sub2api/internal/service"
	"github.com/stretchr/testify/require"
)

type legacyStorePostgresUserRepo struct {
	service.UserRepository
	client *dbent.Client
}

func (r *legacyStorePostgresUserRepo) GetByID(ctx context.Context, id int64) (*service.User, error) {
	rows, err := clientFromContext(ctx, r.client).QueryContext(ctx, `SELECT id,balance FROM users WHERE id=$1`, id)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	if !rows.Next() {
		return nil, service.ErrUserNotFound
	}
	u := &service.User{}
	err = rows.Scan(&u.ID, &u.Balance)
	return u, err
}
func (r *legacyStorePostgresUserRepo) UpdateBalance(ctx context.Context, id int64, amount float64) error {
	_, err := clientFromContext(ctx, r.client).ExecContext(ctx, `UPDATE users SET balance=balance+$2 WHERE id=$1`, id, amount)
	return err
}

type legacyStorePostgresSubRepo struct {
	service.UserSubscriptionRepository
	client *dbent.Client
}
type legacyStorePostgresGroupRepo struct{ service.GroupRepository }

func (legacyStorePostgresGroupRepo) GetByID(_ context.Context, id int64) (*service.Group, error) {
	return &service.Group{ID: id, Status: "active", SubscriptionType: "subscription"}, nil
}
func (r *legacyStorePostgresSubRepo) GetByUserIDAndGroupID(ctx context.Context, userID, groupID int64) (*service.UserSubscription, error) {
	rows, err := clientFromContext(ctx, r.client).QueryContext(ctx, `SELECT id FROM user_subscriptions WHERE user_id=$1 AND group_id=$2 AND deleted_at IS NULL`, userID, groupID)
	if err != nil {
		return nil, err
	}
	var id int64
	if !rows.Next() {
		rows.Close()
		return nil, service.ErrSubscriptionNotFound
	}
	err = rows.Scan(&id)
	rows.Close()
	if err != nil {
		return nil, err
	}
	return r.GetByID(ctx, id)
}
func (r *legacyStorePostgresSubRepo) GetByID(ctx context.Context, id int64) (*service.UserSubscription, error) {
	rows, err := clientFromContext(ctx, r.client).QueryContext(ctx, `SELECT id,user_id,group_id,starts_at,expires_at,status,COALESCE(notes,'') FROM user_subscriptions WHERE id=$1 AND deleted_at IS NULL`, id)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	if !rows.Next() {
		return nil, service.ErrSubscriptionNotFound
	}
	s := &service.UserSubscription{}
	err = rows.Scan(&s.ID, &s.UserID, &s.GroupID, &s.StartsAt, &s.ExpiresAt, &s.Status, &s.Notes)
	return s, err
}

func legacyStorePostgres(t *testing.T) (*sql.DB, *service.SubscriptionService, *service.RedeemService, *service.ActivateSubscriptionRenewalInput) {
	t.Helper()
	db, _, in := renewalPostgres(t)
	_, err := db.Exec(`DROP INDEX renewal_active_sub;
ALTER TABLE users ADD COLUMN IF NOT EXISTS balance NUMERIC NOT NULL DEFAULT 0;
ALTER TABLE redeem_codes ADD COLUMN IF NOT EXISTS notes TEXT;
ALTER TABLE redeem_codes ADD COLUMN IF NOT EXISTS value NUMERIC NOT NULL DEFAULT 1;
ALTER TABLE redeem_codes ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
ALTER TABLE redeem_codes ADD COLUMN IF NOT EXISTS expires_at TIMESTAMPTZ;
UPDATE redeem_codes SET notes='store order o_' || substring(code from 7);`)
	require.NoError(t, err)
	client := dbent.NewClient(dbent.Driver(entsql.OpenDB(dialect.Postgres, db)))
	repo := &legacyStorePostgresSubRepo{UserSubscriptionRepository: NewUserSubscriptionRepository(client), client: client}
	subs := service.NewSubscriptionService(legacyStorePostgresGroupRepo{}, repo, nil, client, nil)
	t.Cleanup(subs.Stop)
	redeem := service.NewRedeemService(NewRedeemCodeRepository(client), &legacyStorePostgresUserRepo{client: client}, subs, nil, nil, client, nil, nil)
	return db, subs, redeem, in
}
func legacyStoreRefund(purchase string, amount float64) *service.RedeemCode {
	sum := sha256.Sum256([]byte("10:" + purchase))
	return &service.RedeemCode{Code: "sbr_" + hex.EncodeToString(sum[:])[:28], Type: service.RedeemTypeBalance, Value: amount, Status: service.StatusUnused, Notes: "subscription refund / o_" + purchase[6:]}
}
func legacyStoreNotes(parent int64, current string, pending ...string) string {
	queue := ""
	for i, code := range pending {
		if i > 0 {
			queue += " "
		}
		queue += code
	}
	return fmt.Sprintf("通过兑换码 %s 兑换\n套餐当前购买码: %s\n套餐待启用购买码: %s\n套餐循环检查自动重开，原订阅ID %d", current, current, queue, parent)
}

func TestLegacyStorePostgresOrdinaryRefundAndReplayRemainValid(t *testing.T) {
	db, subs, redeem, _ := legacyStorePostgres(t)
	ctx := context.Background()
	require.NoError(t, subs.RevokeSubscription(ctx, 1))
	r, err := redeem.CreateAndRedeemStoreCode(ctx, 10, legacyStoreRefund("store_current", 8))
	require.NoError(t, err)
	require.Equal(t, service.StatusUsed, r.Status)
	r, err = redeem.CreateAndRedeemStoreCode(ctx, 10, legacyStoreRefund("store_current", 999))
	require.NoError(t, err)
	require.Equal(t, 8.0, r.Value)
	var balance float64
	require.NoError(t, db.QueryRow(`SELECT balance FROM users WHERE id=10`).Scan(&balance))
	require.Equal(t, 8.0, balance)
	// Unrelated administrator balance grants continue through the normal path.
	require.NoError(t, redeem.CreateCode(ctx, &service.RedeemCode{Code: "ordinary_gift", Type: service.RedeemTypeBalance, Value: 2, Status: service.StatusUnused}))
	_, err = redeem.RedeemForAdminFulfillment(ctx, 10, "ordinary_gift")
	require.NoError(t, err)
	require.NoError(t, db.QueryRow(`SELECT balance FROM users WHERE id=10`).Scan(&balance))
	require.Equal(t, 10.0, balance)
}

func TestLegacyStorePostgresStoreFulfillmentAndReplayRemainAtomic(t *testing.T) {
	db, _, redeem, in := legacyStorePostgres(t)
	ctx := context.Background()
	groupID := int64(20)
	newCode := &service.RedeemCode{Code: "store_new", Type: service.RedeemTypeSubscription, Value: 1, Status: service.StatusUnused, GroupID: &groupID, ValidityDays: 30, Notes: "store order o_new"}
	first, err := redeem.CreateAndRedeemStoreCode(ctx, 10, newCode)
	require.NoError(t, err)
	require.Equal(t, service.StatusUsed, first.Status)
	again, err := redeem.CreateAndRedeemStoreCode(ctx, 10, newCode)
	require.NoError(t, err)
	require.Equal(t, first.ID, again.ID)
	var expires time.Time
	require.NoError(t, db.QueryRow(`SELECT expires_at FROM user_subscriptions WHERE id=1`).Scan(&expires))
	require.True(t, expires.Equal(in.ExpectedExpiresAt.AddDate(0, 0, 30)))
	var count int
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM redeem_codes WHERE code='store_new' AND status='used'`).Scan(&count))
	require.Equal(t, 1, count)
}

func TestLegacyStorePostgresRefundCreditFailureRollsBackAndRetry(t *testing.T) {
	db, subs, redeem, _ := legacyStorePostgres(t)
	ctx := context.Background()
	require.NoError(t, subs.RevokeSubscription(ctx, 1))
	_, err := db.Exec(`ALTER TABLE users ADD CONSTRAINT test_refund_balance_failure CHECK (balance < 5)`)
	require.NoError(t, err)
	_, err = redeem.CreateAndRedeemStoreCode(ctx, 10, legacyStoreRefund("store_current", 8))
	require.ErrorContains(t, err, "update user balance")
	var count int
	var balance float64
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM redeem_codes WHERE code LIKE 'sbr_%'`).Scan(&count))
	require.Zero(t, count, "failure after marking used must roll back both creation and redemption")
	require.NoError(t, db.QueryRow(`SELECT balance FROM users WHERE id=10`).Scan(&balance))
	require.Zero(t, balance)
	_, err = db.Exec(`ALTER TABLE users DROP CONSTRAINT test_refund_balance_failure`)
	require.NoError(t, err)
	result, err := redeem.CreateAndRedeemStoreCode(ctx, 10, legacyStoreRefund("store_current", 8))
	require.NoError(t, err)
	require.Equal(t, service.StatusUsed, result.Status)
	require.NoError(t, db.QueryRow(`SELECT balance FROM users WHERE id=10`).Scan(&balance))
	require.Equal(t, 8.0, balance)
}

func TestLegacyStorePostgresResetBlocksOldRefundsButKeepsUnusedQueueRefund(t *testing.T) {
	db, subs, redeem, in := legacyStorePostgres(t)
	ctx := context.Background()
	receipt, err := subs.ActivateSubscriptionRenewal(ctx, 1, in)
	require.NoError(t, err)
	for _, code := range []string{"store_current", "store_next", "store_queued"} {
		_, err := redeem.CreateAndRedeemStoreCode(ctx, 10, legacyStoreRefund(code, 9))
		require.ErrorContains(t, err, "管理员")
	}
	var refunds int
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM redeem_codes WHERE code LIKE 'sbr_%'`).Scan(&refunds))
	require.Zero(t, refunds, "rejected admin refunds must not leave unused claims")
	// The ordinary user redemption route also protects an already-created code.
	blocked := legacyStoreRefund("store_next", 9)
	require.NoError(t, redeem.CreateCode(ctx, blocked))
	_, err = redeem.Redeem(ctx, 10, blocked.Code)
	require.ErrorContains(t, err, "应退金额")
	var balance float64
	require.NoError(t, db.QueryRow(`SELECT balance FROM users WHERE id=10`).Scan(&balance))
	require.Zero(t, balance)
	require.NoError(t, subs.RevokeSubscription(ctx, receipt.NewSubscriptionID))
	_, err = redeem.CreateAndRedeemStoreCode(ctx, 10, legacyStoreRefund("store_queued", 7))
	require.NoError(t, err)
	_, err = redeem.CreateAndRedeemStoreCode(ctx, 10, legacyStoreRefund("store_next", 9))
	require.ErrorContains(t, err, "应退金额")
	require.NoError(t, db.QueryRow(`SELECT balance FROM users WHERE id=10`).Scan(&balance))
	require.Equal(t, 7.0, balance)
}

func TestLegacyStorePostgresCronCannotReissueRevokedSuccessorAcrossReset(t *testing.T) {
	db, subs, _, in := legacyStorePostgres(t)
	ctx := context.Background()
	require.NoError(t, subs.RevokeSubscription(ctx, 1))
	assign := &service.AssignSubscriptionInput{UserID: 10, GroupID: 20, ValidityDays: 61, Notes: legacyStoreNotes(1, "store_next", "store_queued")}
	issued, err := subs.AssignSubscription(ctx, assign)
	require.NoError(t, err)
	next := *in
	next.CurrentCode = "store_next"
	next.NextCode = "store_queued"
	next.PendingCodes = nil
	next.IdempotencyKey = "legacy-following-reset"
	next.ExpectedStartsAt = issued.StartsAt
	next.ExpectedExpiresAt = issued.ExpiresAt
	next.ExpectedNotes = issued.Notes
	receipt, err := subs.ActivateSubscriptionRenewal(ctx, issued.ID, &next)
	require.NoError(t, err)
	require.NoError(t, subs.RevokeSubscription(ctx, receipt.NewSubscriptionID))
	again, err := subs.AssignSubscription(ctx, assign)
	require.NoError(t, err)
	require.Equal(t, issued.ID, again.ID)
	var active, total int
	require.NoError(t, db.QueryRow(`SELECT count(*) FILTER (WHERE deleted_at IS NULL),count(*) FROM user_subscriptions`).Scan(&active, &total))
	require.Zero(t, active)
	require.Equal(t, 3, total)
}

func TestLegacyStorePostgresCronRejectsReorderedPurchases(t *testing.T) {
	db, subs, _, _ := legacyStorePostgres(t)
	ctx := context.Background()
	require.NoError(t, subs.RevokeSubscription(ctx, 1))
	_, err := subs.AssignSubscription(ctx, &service.AssignSubscriptionInput{UserID: 10, GroupID: 20, ValidityDays: 61, Notes: legacyStoreNotes(1, "store_queued", "store_next")})
	require.ErrorContains(t, err, "最早")
	var count int
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM user_subscriptions`).Scan(&count))
	require.Equal(t, 1, count)
}

func TestLegacyStorePostgresCronRejectsUnrelatedRevokedPurchaseCopy(t *testing.T) {
	db, subs, _, _ := legacyStorePostgres(t)
	ctx := context.Background()
	require.NoError(t, subs.RevokeSubscription(ctx, 1))
	_, err := db.Exec(`INSERT INTO user_subscriptions (user_id,group_id,starts_at,expires_at,status,notes,deleted_at) VALUES (10,20,NOW(),NOW()+INTERVAL '31 days','active','通过兑换码 store_next 兑换',NOW())`)
	require.NoError(t, err)
	_, err = subs.AssignSubscription(ctx, &service.AssignSubscriptionInput{UserID: 10, GroupID: 20, ValidityDays: 61, Notes: legacyStoreNotes(1, "store_next", "store_queued")})
	require.ErrorContains(t, err, "其他历史套餐")
	var count int
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM user_subscriptions`).Scan(&count))
	require.Equal(t, 2, count)
}

func TestLegacyStorePostgresCronRejectsUnrelatedLiveRowWithoutUniqueIndex(t *testing.T) {
	db, subs, _, _ := legacyStorePostgres(t)
	ctx := context.Background()
	require.NoError(t, subs.RevokeSubscription(ctx, 1))
	var otherID int64
	var originalExpiry time.Time
	require.NoError(t, db.QueryRow(`INSERT INTO user_subscriptions (user_id,group_id,starts_at,expires_at,status,notes) VALUES (10,20,NOW(),NOW()+INTERVAL '7 days','active','管理员单独赠送') RETURNING id,expires_at`).Scan(&otherID, &originalExpiry))
	var uniqueIndexes int
	require.NoError(t, db.QueryRow(`SELECT count(*) FROM pg_indexes WHERE schemaname=current_schema() AND tablename='user_subscriptions' AND indexdef LIKE 'CREATE UNIQUE INDEX%' AND indexname<>'user_subscriptions_pkey'`).Scan(&uniqueIndexes))
	require.Zero(t, uniqueIndexes, "production has no live user/group unique index")
	_, err := subs.AssignSubscription(ctx, &service.AssignSubscriptionInput{UserID: 10, GroupID: 20, ValidityDays: 61, Notes: legacyStoreNotes(1, "store_next", "store_queued")})
	require.ErrorContains(t, err, "已有其他有效套餐")
	var active, total int
	require.NoError(t, db.QueryRow(`SELECT count(*) FILTER (WHERE deleted_at IS NULL),count(*) FROM user_subscriptions`).Scan(&active, &total))
	require.Equal(t, 1, active)
	require.Equal(t, 2, total)
	var expiry time.Time
	var notes string
	require.NoError(t, db.QueryRow(`SELECT expires_at,notes FROM user_subscriptions WHERE id=$1`, otherID).Scan(&expiry, &notes))
	require.Equal(t, originalExpiry, expiry)
	require.Equal(t, "管理员单独赠送", notes)
}

func TestLegacyStorePostgresConcurrentCronAndAdminGrantWithoutUniqueIndex(t *testing.T) {
	db, subs, _, _ := legacyStorePostgres(t)
	setupCtx, setupCancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer setupCancel()
	require.NoError(t, subs.RevokeSubscription(setupCtx, 1))
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	start := make(chan struct{})
	errs := make([]error, 2)
	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		<-start
		_, errs[0] = subs.AssignSubscription(ctx, &service.AssignSubscriptionInput{UserID: 10, GroupID: 20, ValidityDays: 61, Notes: legacyStoreNotes(1, "store_next", "store_queued")})
	}()
	go func() {
		defer wg.Done()
		<-start
		_, errs[1] = subs.AssignSubscription(ctx, &service.AssignSubscriptionInput{UserID: 10, GroupID: 20, ValidityDays: 7, Notes: "管理员并发赠送"})
	}()
	close(start)
	wg.Wait()
	require.NoError(t, ctx.Err(), "contenders did not finish: cron=%v admin=%v", errs[0], errs[1])
	require.NotEqual(t, errs[0] == nil, errs[1] == nil, "only one subscription may be created")
	for _, err := range errs {
		require.NotErrorIs(t, err, context.Canceled)
		require.NotErrorIs(t, err, context.DeadlineExceeded)
		if err != nil {
			require.True(t, infraerrors.IsConflict(err), "loser must be a domain conflict: %v", err)
		}
	}
	assertCtx, assertCancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer assertCancel()
	var active, total int
	require.NoError(t, db.QueryRowContext(assertCtx, `SELECT count(*) FILTER (WHERE deleted_at IS NULL),count(*) FROM user_subscriptions`).Scan(&active, &total))
	require.Equal(t, 1, active)
	require.Equal(t, 2, total)
}

func TestLegacyStorePostgresConcurrentCronAndRefundClaimExactlyOnce(t *testing.T) {
	db, subs, redeem, _ := legacyStorePostgres(t)
	setupCtx, setupCancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer setupCancel()
	require.NoError(t, subs.RevokeSubscription(setupCtx, 1))
	setupCancel()
	// Both contenders perform multiple round trips through the optional SSH
	// tunnel. Start their deadline after setup and allow serialized completion;
	// a timeout or database deadlock must still fail, never count as the loser.
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	start := make(chan struct{})
	errs := make([]error, 2)
	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		<-start
		_, errs[0] = subs.AssignSubscription(ctx, &service.AssignSubscriptionInput{UserID: 10, GroupID: 20, ValidityDays: 61, Notes: legacyStoreNotes(1, "store_next", "store_queued")})
	}()
	go func() {
		defer wg.Done()
		<-start
		_, errs[1] = redeem.CreateAndRedeemStoreCode(ctx, 10, legacyStoreRefund("store_next", 9))
	}()
	close(start)
	wg.Wait()
	require.NoError(t, ctx.Err(), "contenders did not finish: cron=%v refund=%v", errs[0], errs[1])
	require.NotEqual(t, errs[0] == nil, errs[1] == nil, "exactly one claimant must succeed")
	for _, err := range errs {
		require.NotErrorIs(t, err, context.Canceled)
		require.NotErrorIs(t, err, context.DeadlineExceeded)
		if err != nil {
			require.True(t, infraerrors.IsConflict(err), "loser must be a rights conflict, not a database error: %v", err)
		}
	}
	assertCtx, assertCancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer assertCancel()
	var balance float64
	var active int
	require.NoError(t, db.QueryRowContext(assertCtx, `SELECT balance FROM users WHERE id=10`).Scan(&balance))
	require.NoError(t, db.QueryRowContext(assertCtx, `SELECT count(*) FROM user_subscriptions WHERE deleted_at IS NULL`).Scan(&active))
	if errs[0] == nil {
		require.Zero(t, balance)
		require.Equal(t, 1, active)
	} else {
		require.Equal(t, 9.0, balance)
		require.Zero(t, active)
	}
}

func TestLegacyStorePostgresConcurrentResetNeverCreditsStaleRefund(t *testing.T) {
	db, subs, redeem, in := legacyStorePostgres(t)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	start := make(chan struct{})
	errs := make([]error, 2)
	var wg sync.WaitGroup
	wg.Add(2)
	go func() { defer wg.Done(); <-start; _, errs[0] = subs.ActivateSubscriptionRenewal(ctx, 1, in) }()
	go func() {
		defer wg.Done()
		<-start
		_, errs[1] = redeem.CreateAndRedeemStoreCode(ctx, 10, legacyStoreRefund("store_next", 9))
	}()
	close(start)
	wg.Wait()
	require.NoError(t, ctx.Err())
	require.NoError(t, errs[0])
	require.Error(t, errs[1])
	var balance float64
	require.NoError(t, db.QueryRow(`SELECT balance FROM users WHERE id=10`).Scan(&balance))
	require.Zero(t, balance)
}
