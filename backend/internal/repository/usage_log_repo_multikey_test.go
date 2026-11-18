package repository

import (
	"context"
	"database/sql/driver"
	"regexp"
	"testing"
	"time"

	"github.com/DATA-DOG/go-sqlmock"
	"github.com/Wei-Shaw/sub2api/internal/pkg/usagestats"
	"github.com/stretchr/testify/require"
)

func TestBuildUsageLogFilterConditionsMultiKeyAndReasoningEffort(t *testing.T) {
	conditions, args := buildUsageLogFilterConditions(usagestats.UsageLogFilters{
		UserID:          42,
		APIKeyIDs:       []int64{7, 2, 7},
		ReasoningEffort: "HIGH",
	}, "ul")

	require.Equal(t, []string{
		"ul.user_id = $1",
		"ul.api_key_id IN ($2, $3)",
		"LOWER(COALESCE(NULLIF(TRIM(ul.reasoning_effort), ''), '')) = $4",
	}, conditions)
	require.Equal(t, []any{int64(42), int64(7), int64(2), "high"}, args)
}

func TestBuildUsageLogFilterConditionsUnsetReasoningEffort(t *testing.T) {
	conditions, args := buildUsageLogFilterConditions(usagestats.UsageLogFilters{
		ReasoningEffort: "__unset__",
	}, "")

	require.Equal(t, []string{
		"LOWER(COALESCE(NULLIF(TRIM(reasoning_effort), ''), '')) = ''",
	}, conditions)
	require.Empty(t, args)
}

func TestGetStatsWithFiltersReturnsKeyGroupAndEffortBreakdowns(t *testing.T) {
	db, mock := newSQLMock(t)
	repo := &usageLogRepository{sql: db}
	filters := usagestats.UsageLogFilters{
		UserID:            42,
		APIKeyIDs:         []int64{2, 7},
		ReasoningEffort:   "high",
		IncludeBreakdowns: true,
	}
	args := []driver.Value{int64(42), int64(2), int64(7), "high"}

	mock.ExpectQuery(`(?s)FROM usage_logs\s+WHERE user_id = \$1 AND api_key_id IN \(\$2, \$3\).*reasoning_effort.*= \$4.*GROUP BY GROUPING SETS`).
		WithArgs(args...).
		WillReturnRows(sqlmock.NewRows([]string{
			"inbound_grouped", "upstream_grouped", "inbound_endpoint", "upstream_endpoint",
			"requests", "input_tokens", "output_tokens", "cache_creation_tokens", "cache_read_tokens",
			"cost", "actual_cost", "account_cost", "avg_duration_ms",
		}).AddRow(1, 1, nil, nil, int64(3), int64(100), int64(200), int64(10), int64(40), 0.30, 0.24, 0.20, 80.0))

	mock.ExpectQuery(`(?s)FROM usage_logs ul\s+WHERE ul.user_id = \$1 AND ul.api_key_id IN \(\$2, \$3\).*ul.reasoning_effort.*= \$4\s+GROUP BY reasoning_effort`).
		WithArgs(args...).
		WillReturnRows(sqlmock.NewRows([]string{
			"reasoning_effort", "requests", "input_tokens", "output_tokens",
			"cache_creation_tokens", "cache_read_tokens", "total_tokens", "cost", "actual_cost",
		}).AddRow("high", int64(3), int64(100), int64(200), int64(10), int64(40), int64(350), 0.30, 0.24))

	mock.ExpectQuery(`(?s)FROM usage_logs ul\s+LEFT JOIN api_keys k.*LEFT JOIN groups g.*WHERE ul.user_id = \$1 AND ul.api_key_id IN \(\$2, \$3\).*GROUP BY ul.api_key_id, k.name, ul.group_id, g.name`).
		WithArgs(args...).
		WillReturnRows(sqlmock.NewRows([]string{
			"api_key_id", "api_key_name", "group_id", "group_name", "requests",
			"input_tokens", "output_tokens", "cache_creation_tokens", "cache_read_tokens",
			"total_tokens", "cost", "actual_cost",
		}).
			AddRow(int64(2), "team-key", int64(1), "default", int64(2), int64(70), int64(140), int64(5), int64(20), int64(235), 0.20, 0.16).
			AddRow(int64(2), "team-key", int64(9), "vip", int64(1), int64(30), int64(60), int64(5), int64(20), int64(115), 0.10, 0.08))

	stats, err := repo.GetStatsWithFilters(context.Background(), filters)
	require.NoError(t, err)
	require.Equal(t, int64(3), stats.TotalRequests)
	require.InDelta(t, 0.24, stats.TotalActualCost, 0.000001)
	require.Len(t, stats.ReasoningEfforts, 1)
	require.Equal(t, "high", stats.ReasoningEfforts[0].ReasoningEffort)
	require.Len(t, stats.APIKeys, 2, "the same key must keep separate usage rows when its group changed")
	require.Equal(t, int64(1), stats.APIKeys[0].GroupID)
	require.Equal(t, "default", stats.APIKeys[0].GroupName)
	require.Equal(t, int64(9), stats.APIKeys[1].GroupID)
	require.Equal(t, "vip", stats.APIKeys[1].GroupName)
	require.NoError(t, mock.ExpectationsWereMet())
}

func TestModelStatsAccountAndMultiKeyFiltersKeepChargedAndAccountCostsSeparate(t *testing.T) {
	start := time.Date(2026, 11, 18, 8, 30, 0, 0, time.UTC)
	end := start.Add(time.Hour)
	for _, tc := range []struct {
		name    string
		filters UsageLogFilters
		where   string
		args    []driver.Value
	}{
		{
			name:    "account_only",
			filters: UsageLogFilters{AccountID: 42},
			where:   "ul.account_id = $3",
			args:    []driver.Value{start, end, int64(42)},
		},
		{
			name: "account_and_multiple_keys_with_effort",
			filters: UsageLogFilters{
				AccountID: 42, APIKeyIDs: []int64{2, 7, 2}, ReasoningEffort: "HIGH",
			},
			where: "ul.api_key_id IN ($3, $4) AND ul.account_id = $5 AND LOWER(COALESCE(NULLIF(TRIM(ul.reasoning_effort), ''), '')) = $6",
			args:  []driver.Value{start, end, int64(2), int64(7), int64(42), "high"},
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			db, mock := newSQLMock(t)
			repo := &usageLogRepository{sql: db}
			// Check the actual SQL aggregates, not just the returned field names:
			// account-only filtering must never replace customer charges with
			// the independent account cost, and multi-key/effort filters still apply.
			costs := regexp.QuoteMeta("COALESCE(SUM(ul.actual_cost), 0) as actual_cost,") + `\s+` +
				regexp.QuoteMeta("COALESCE(SUM(COALESCE(ul.account_stats_cost, ul.total_cost) * COALESCE(ul.account_rate_multiplier, 1)), 0) as account_cost")
			where := regexp.QuoteMeta("WHERE ul.created_at >= $1 AND ul.created_at < $2 AND " + tc.where)
			mock.ExpectQuery(`(?s)` + costs + `\s+FROM usage_logs ul\s+` + where + `\s+GROUP BY`).
				WithArgs(tc.args...).
				WillReturnRows(sqlmock.NewRows([]string{
					"model", "requests", "input_tokens", "output_tokens",
					"cache_creation_tokens", "cache_read_tokens", "total_tokens",
					"cost", "actual_cost", "account_cost",
				}).AddRow("gemini-4-carbon", int64(3), int64(100), int64(200), int64(10), int64(40), int64(350), 6.0, 2.4, 0.9))

			rows, err := repo.GetModelStatsWithUsageFiltersBySource(context.Background(), start, end, tc.filters, usagestats.ModelSourceRequested)
			require.NoError(t, err)
			require.Len(t, rows, 1)
			require.Equal(t, 6.0, rows[0].Cost)
			require.Equal(t, 2.4, rows[0].ActualCost)
			require.Equal(t, 0.9, rows[0].AccountCost)
			require.NoError(t, mock.ExpectationsWereMet())
		})
	}
}
