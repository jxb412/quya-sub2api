package repository

import (
	"database/sql"
	"fmt"
	"strings"
	"sync"
	"time"

	dbent "github.com/Wei-Shaw/sub2api/ent"
	"github.com/Wei-Shaw/sub2api/internal/pkg/usagestats"
	"github.com/Wei-Shaw/sub2api/internal/service"
	gocache "github.com/patrickmn/go-cache"
)

const rawUsageLogModelColumn = "model"

// rawUsageLogModelColumn preserves the exact stored usage_logs.model semantics for direct filters.
// Historical rows may contain upstream/billing model values, while newer rows store requested_model.
// Requested/upstream/mapping analytics must use resolveModelDimensionExpression instead.

// usageLogSuccessFilterUL 用于把"失败请求 usage log"（tokens=0、cost=0、不计费的占位记录）
// 从统计性聚合中排除，避免污染 Dashboard / 用量拆分等指标。
//
// schema 中没有 success bool 列；新增列要做迁移，风险大；这里用 actual_cost > 0 作为代理：
// 任何成功落账的请求都会产生 actual_cost（包括 token 计费、纯图片 token 计费、按次/按图计费），
// 反之 failed-request usage log 的 actual_cost 为 0。
// 早期版本用 4 项 token 和 > 0 判定会把"按次/按图计费"与"image_output_tokens 独立计费"的纯图片
// 请求误判为失败，导致这部分请求从用量统计里消失，故改用 actual_cost。
// 配合 `FROM usage_logs ul` JOIN 查询使用。
const usageLogSuccessFilterUL = "ul.actual_cost > 0"

// usageLogEffectivePlatformExpr 用于按"有效平台"维度聚合 usage_logs：
// 优先取请求实际走的分组 platform，若分组未设置 platform 再 fallback 到 account.platform。
// Composite groups are a routing layer, so platform analytics must use the
// resolved concrete account platform instead of grouping spend under "composite".
// 配套要求查询里 LEFT JOIN groups g ON g.id = ul.group_id 与 LEFT JOIN accounts a ON a.id = ul.account_id。
const usageLogEffectivePlatformExpr = "CASE WHEN g.platform = 'composite' THEN a.platform ELSE COALESCE(NULLIF(g.platform,''), a.platform) END"

// dateFormatWhitelist 将 granularity 参数映射为 PostgreSQL TO_CHAR 格式字符串，防止外部输入直接拼入 SQL
var dateFormatWhitelist = map[string]string{
	"hour":  "YYYY-MM-DD HH24:00",
	"day":   "YYYY-MM-DD",
	"week":  "IYYY-IW",
	"month": "YYYY-MM",
}

// safeDateFormat 根据白名单获取 dateFormat，未匹配时返回默认值
func safeDateFormat(granularity string) string {
	if f, ok := dateFormatWhitelist[granularity]; ok {
		return f
	}
	return "YYYY-MM-DD"
}

func appendUsageLogBillingModeWhereCondition(conditions []string, args []any, billingMode string) ([]string, []any) {
	return appendUsageLogBillingModeWhereConditionWithAlias(conditions, args, billingMode, "")
}

func appendUsageLogBillingModeWhereConditionWithAlias(conditions []string, args []any, billingMode string, alias string) ([]string, []any) {
	mode := strings.TrimSpace(billingMode)
	if mode == "" {
		return conditions, args
	}
	column := func(name string) string {
		if alias == "" {
			return name
		}
		return alias + "." + name
	}
	placeholder := fmt.Sprintf("$%d", len(args)+1)
	switch service.BillingMode(mode) {
	case service.BillingModeImage:
		conditions = append(conditions, fmt.Sprintf("(%s = %s OR ((%s IS NULL OR %s = '') AND COALESCE(%s, 0) > 0))", column("billing_mode"), placeholder, column("billing_mode"), column("billing_mode"), column("image_count")))
	case service.BillingModeVideo:
		conditions = append(conditions, fmt.Sprintf("%s = %s", column("billing_mode"), placeholder))
	case service.BillingModeToken:
		conditions = append(conditions, fmt.Sprintf("(%s = %s OR ((%s IS NULL OR %s = '') AND COALESCE(%s, 0) <= 0))", column("billing_mode"), placeholder, column("billing_mode"), column("billing_mode"), column("image_count")))
	default:
		conditions = append(conditions, fmt.Sprintf("%s = %s", column("billing_mode"), placeholder))
	}
	args = append(args, mode)
	return conditions, args
}

func appendUsageLogBillingModeQueryFilter(query string, args []any, billingMode string, alias string) (string, []any) {
	conditions, args := appendUsageLogBillingModeWhereConditionWithAlias(nil, args, billingMode, alias)
	if len(conditions) == 0 {
		return query, args
	}
	return query + " AND " + conditions[0], args
}

// appendRawUsageLogModelQueryFilter keeps direct model filters on the raw model column for backward
// compatibility with historical rows. Requested/upstream analytics must use
// resolveModelDimensionExpression instead.
func appendRawUsageLogModelQueryFilter(query string, args []any, model string) (string, []any) {
	if strings.TrimSpace(model) == "" {
		return query, args
	}
	query += fmt.Sprintf(" AND %s = $%d", rawUsageLogModelColumn, len(args)+1)
	args = append(args, model)
	return query, args
}

func appendUsageLogModelQueryFilter(query string, args []any, model string, source string) (string, []any) {
	if strings.TrimSpace(source) == "" {
		return appendRawUsageLogModelQueryFilter(query, args, model)
	}
	if strings.TrimSpace(model) == "" {
		return query, args
	}
	query += fmt.Sprintf(" AND %s = $%d", resolveModelDimensionExpression(source), len(args)+1)
	args = append(args, model)
	return query, args
}

// buildUsageLogFilterConditions builds the shared WHERE fragments used by the
// usage list, aggregate, trend and chart queries. Keeping these in one place
// prevents multi-key / reasoning-effort filters from silently dropping out of
// one of the dashboard surfaces.
func buildUsageLogFilterConditions(filters usagestats.UsageLogFilters, alias string) ([]string, []any) {
	return buildUsageLogFilterConditionsWithArgs(filters, alias, nil)
}

func buildUsageLogFilterConditionsWithArgs(filters usagestats.UsageLogFilters, alias string, args []any) ([]string, []any) {
	column := func(name string) string {
		if alias == "" {
			return name
		}
		return alias + "." + name
	}
	conditions := make([]string, 0, 16)
	if args == nil {
		args = make([]any, 0, 16)
	}

	if filters.UserID > 0 {
		conditions = append(conditions, fmt.Sprintf("%s = $%d", column("user_id"), len(args)+1))
		args = append(args, filters.UserID)
	}
	keyIDs := make([]int64, 0, len(filters.APIKeyIDs))
	seen := make(map[int64]struct{}, len(filters.APIKeyIDs))
	for _, id := range filters.APIKeyIDs {
		if id <= 0 {
			continue
		}
		if _, ok := seen[id]; ok {
			continue
		}
		seen[id] = struct{}{}
		keyIDs = append(keyIDs, id)
	}
	if len(keyIDs) > 0 {
		placeholders := make([]string, 0, len(keyIDs))
		for _, id := range keyIDs {
			placeholders = append(placeholders, fmt.Sprintf("$%d", len(args)+1))
			args = append(args, id)
		}
		conditions = append(conditions, fmt.Sprintf("%s IN (%s)", column("api_key_id"), strings.Join(placeholders, ", ")))
	} else if filters.APIKeyID > 0 {
		conditions = append(conditions, fmt.Sprintf("%s = $%d", column("api_key_id"), len(args)+1))
		args = append(args, filters.APIKeyID)
	}
	if filters.AccountID > 0 {
		conditions = append(conditions, fmt.Sprintf("%s = $%d", column("account_id"), len(args)+1))
		args = append(args, filters.AccountID)
	}
	if filters.GroupID > 0 {
		conditions = append(conditions, fmt.Sprintf("%s = $%d", column("group_id"), len(args)+1))
		args = append(args, filters.GroupID)
	}
	if requestID := strings.TrimSpace(filters.RequestID); requestID != "" {
		conditions = append(conditions, fmt.Sprintf("%s = $%d", column("request_id"), len(args)+1))
		args = append(args, requestID)
	}
	if model := strings.TrimSpace(filters.Model); model != "" {
		modelExpr := column(rawUsageLogModelColumn)
		if strings.TrimSpace(filters.ModelFilterSource) != "" {
			modelExpr = resolveModelDimensionExpressionWithAlias(filters.ModelFilterSource, alias)
		}
		conditions = append(conditions, fmt.Sprintf("%s = $%d", modelExpr, len(args)+1))
		args = append(args, model)
	}
	if filters.RequestType != nil {
		condition, conditionArgs := buildRequestTypeFilterConditionWithAlias(len(args)+1, *filters.RequestType, alias)
		conditions = append(conditions, condition)
		args = append(args, conditionArgs...)
	} else if filters.Stream != nil {
		conditions = append(conditions, fmt.Sprintf("%s = $%d", column("stream"), len(args)+1))
		args = append(args, *filters.Stream)
	}
	conditions, args = appendNativeCompactionV2WhereCondition(conditions, args, filters.NativeCompactionV2, alias)
	if filters.BillingType != nil {
		conditions = append(conditions, fmt.Sprintf("%s = $%d", column("billing_type"), len(args)+1))
		args = append(args, int16(*filters.BillingType))
	}
	conditions, args = appendUsageLogBillingModeWhereConditionWithAlias(conditions, args, filters.BillingMode, alias)
	if filters.UpstreamModelMismatch != nil {
		conditions = append(conditions, upstreamModelMismatchCondition(column("upstream_model_mismatch"), *filters.UpstreamModelMismatch))
	}
	if effort := strings.ToLower(strings.TrimSpace(filters.ReasoningEffort)); effort != "" {
		effortExpr := fmt.Sprintf("LOWER(COALESCE(NULLIF(TRIM(%s), ''), ''))", column("reasoning_effort"))
		if effort == "__unset__" {
			conditions = append(conditions, effortExpr+" = ''")
		} else {
			conditions = append(conditions, fmt.Sprintf("%s = $%d", effortExpr, len(args)+1))
			args = append(args, effort)
		}
	}
	if filters.StartTime != nil {
		conditions = append(conditions, fmt.Sprintf("%s >= $%d", column("created_at"), len(args)+1))
		args = append(args, *filters.StartTime)
	}
	if filters.EndTime != nil {
		conditions = append(conditions, fmt.Sprintf("%s < $%d", column("created_at"), len(args)+1))
		args = append(args, *filters.EndTime)
	}
	return conditions, args
}

type usageLogRepository struct {
	client *dbent.Client
	sql    sqlExecutor
	db     *sql.DB

	createBatchOnce     sync.Once
	createBatchCh       chan usageLogCreateRequest
	bestEffortBatchOnce sync.Once
	bestEffortBatchCh   chan usageLogBestEffortRequest
	bestEffortRecent    *gocache.Cache
}

func NewUsageLogRepository(client *dbent.Client, sqlDB *sql.DB) service.UsageLogRepository {
	return newUsageLogRepositoryWithSQL(client, sqlDB)
}

func newUsageLogRepositoryWithSQL(client *dbent.Client, sqlq sqlExecutor) *usageLogRepository {
	// 使用 scanSingleRow 替代 QueryRowContext，保证 ent.Tx 作为 sqlExecutor 可用。
	repo := &usageLogRepository{client: client, sql: sqlq}
	if db, ok := sqlq.(*sql.DB); ok {
		repo.db = db
	}
	repo.bestEffortRecent = gocache.New(usageLogBestEffortRecentTTL, time.Minute)
	return repo
}

func buildWhere(conditions []string) string {
	if len(conditions) == 0 {
		return ""
	}
	return "WHERE " + strings.Join(conditions, " AND ")
}

func appendRequestTypeOrStreamQueryFilter(query string, args []any, requestType *int16, stream *bool) (string, []any) {
	if requestType != nil {
		condition, conditionArgs := buildRequestTypeFilterCondition(len(args)+1, *requestType)
		query += " AND " + condition
		args = append(args, conditionArgs...)
		return query, args
	}
	if stream != nil {
		query += fmt.Sprintf(" AND stream = $%d", len(args)+1)
		args = append(args, *stream)
	}
	return query, args
}

func appendNativeCompactionV2WhereCondition(conditions []string, args []any, nativeCompactionV2 *bool, alias string) ([]string, []any) {
	if nativeCompactionV2 == nil {
		return conditions, args
	}
	column := "native_compaction_v2"
	if alias != "" {
		column = alias + "." + column
	}
	conditions = append(conditions, fmt.Sprintf("%s = $%d", column, len(args)+1))
	args = append(args, *nativeCompactionV2)
	return conditions, args
}

func appendNativeCompactionV2QueryFilter(query string, args []any, nativeCompactionV2 *bool, alias string) (string, []any) {
	conditions, args := appendNativeCompactionV2WhereCondition(nil, args, nativeCompactionV2, alias)
	if len(conditions) == 0 {
		return query, args
	}
	return query + " AND " + conditions[0], args
}

// buildRequestTypeFilterCondition 在 request_type 过滤时兼容 legacy 字段，避免历史数据漏查。
func buildRequestTypeFilterCondition(startArgIndex int, requestType int16) (string, []any) {
	return buildRequestTypeFilterConditionWithAlias(startArgIndex, requestType, "")
}

func buildRequestTypeFilterConditionWithAlias(startArgIndex int, requestType int16, alias string) (string, []any) {
	normalized := service.RequestTypeFromInt16(requestType)
	requestTypeArg := int16(normalized)
	prefix := ""
	if alias != "" {
		prefix = alias + "."
	}
	switch normalized {
	case service.RequestTypeSync:
		return fmt.Sprintf("(%srequest_type = $%d OR (%srequest_type = %d AND %sstream = FALSE AND %sopenai_ws_mode = FALSE))", prefix, startArgIndex, prefix, int16(service.RequestTypeUnknown), prefix, prefix), []any{requestTypeArg}
	case service.RequestTypeStream:
		return fmt.Sprintf("(%srequest_type = $%d OR (%srequest_type = %d AND %sstream = TRUE AND %sopenai_ws_mode = FALSE))", prefix, startArgIndex, prefix, int16(service.RequestTypeUnknown), prefix, prefix), []any{requestTypeArg}
	case service.RequestTypeWSV2:
		return fmt.Sprintf("(%srequest_type = $%d OR (%srequest_type = %d AND %sopenai_ws_mode = TRUE))", prefix, startArgIndex, prefix, int16(service.RequestTypeUnknown), prefix), []any{requestTypeArg}
	default:
		return fmt.Sprintf("%srequest_type = $%d", prefix, startArgIndex), []any{requestTypeArg}
	}
}
