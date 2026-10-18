package handler

import (
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/stretchr/testify/require"
)

func TestUserUsageListParsesExactMinuteRange(t *testing.T) {
	repo := &userUsageRepoCapture{}
	router := newUserUsageTestRouter(repo, nil)

	req := httptest.NewRequest(http.MethodGet, "/usage?start_date=2026-03-08T10:30&end_date=2026-03-08T11:45&timezone=Asia/Shanghai", nil)
	recorder := httptest.NewRecorder()
	router.ServeHTTP(recorder, req)

	require.Equal(t, http.StatusOK, recorder.Code)
	require.NotNil(t, repo.listFilters.StartTime)
	require.NotNil(t, repo.listFilters.EndTime)
	require.Equal(t, "2026-03-08T10:30:00+08:00", repo.listFilters.StartTime.Format(time.RFC3339))
	// The selected end minute is inclusive: the query uses a half-open bound.
	require.Equal(t, "2026-03-08T11:46:00+08:00", repo.listFilters.EndTime.Format(time.RFC3339))
}

func TestUserUsageListKeepsWholeDayRangeCompatibility(t *testing.T) {
	repo := &userUsageRepoCapture{}
	router := newUserUsageTestRouter(repo, nil)

	req := httptest.NewRequest(http.MethodGet, "/usage?start_date=2026-03-08&end_date=2026-03-08&timezone=Asia/Shanghai", nil)
	recorder := httptest.NewRecorder()
	router.ServeHTTP(recorder, req)

	require.Equal(t, http.StatusOK, recorder.Code)
	require.NotNil(t, repo.listFilters.StartTime)
	require.NotNil(t, repo.listFilters.EndTime)
	require.Equal(t, "2026-03-08T00:00:00+08:00", repo.listFilters.StartTime.Format(time.RFC3339))
	require.Equal(t, "2026-03-09T00:00:00+08:00", repo.listFilters.EndTime.Format(time.RFC3339))
}

func TestUserUsageListRejectsReversedRange(t *testing.T) {
	repo := &userUsageRepoCapture{}
	router := newUserUsageTestRouter(repo, nil)

	req := httptest.NewRequest(http.MethodGet, "/usage?start_date=2026-03-08T11:45&end_date=2026-03-08T10:30&timezone=Asia/Shanghai", nil)
	recorder := httptest.NewRecorder()
	router.ServeHTTP(recorder, req)

	require.Equal(t, http.StatusBadRequest, recorder.Code)
}
