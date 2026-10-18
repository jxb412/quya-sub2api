package timezone

import (
	"testing"
	"time"

	"github.com/stretchr/testify/require"
)

func TestParseDateOrDateTime(t *testing.T) {
	const userTZ = "Asia/Shanghai"

	tests := []struct {
		name      string
		value     string
		precision DatePrecision
		expected  string
	}{
		{name: "calendar date", value: "2026-03-08", precision: PrecisionDay, expected: "2026-03-08T00:00:00+08:00"},
		{name: "minute precision", value: "2026-03-08T10:30", precision: PrecisionMinute, expected: "2026-03-08T10:30:00+08:00"},
		{name: "second precision", value: "2026-03-08T10:30:45", precision: PrecisionSecond, expected: "2026-03-08T10:30:45+08:00"},
		{name: "space separator", value: "2026-03-08 10:30", precision: PrecisionMinute, expected: "2026-03-08T10:30:00+08:00"},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			parsed, precision, err := ParseDateOrDateTime(tc.value, userTZ)
			require.NoError(t, err)
			require.Equal(t, tc.precision, precision)
			require.Equal(t, tc.expected, parsed.Format(time.RFC3339))
		})
	}
}

func TestParseDateOrDateTimeRejectsInvalidValues(t *testing.T) {
	for _, value := range []string{"", "10:30", "2026-13-01", "2026-03-08T25:00"} {
		t.Run(value, func(t *testing.T) {
			_, _, err := ParseDateOrDateTime(value, "Asia/Shanghai")
			require.Error(t, err)
		})
	}
}
