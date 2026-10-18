package timezone

import (
	"fmt"
	"strings"
	"time"
)

// DatePrecision describes how much time information a parsed boundary carried.
type DatePrecision int

const (
	// PrecisionDay means the value was a calendar date (YYYY-MM-DD).
	PrecisionDay DatePrecision = iota
	// PrecisionMinute means the value was a timestamp with minute precision.
	PrecisionMinute
	// PrecisionSecond means the value was a timestamp with second precision.
	PrecisionSecond
)

const (
	dateTimeMinuteLayout = "2006-01-02T15:04"
	dateTimeSecondLayout = "2006-01-02T15:04:05"
)

// ParseDateOrDateTime parses a range boundary. It accepts a calendar date
// (YYYY-MM-DD) or an exact local timestamp (YYYY-MM-DDTHH:mm, optionally with
// seconds). A space separator is also accepted so that "2026-01-02 10:30"
// keeps working when a caller forgets to encode the "T".
//
// The returned precision tells the caller whether an end boundary still has to
// be expanded to cover the whole selected day, minute, or second.
func ParseDateOrDateTime(value, userTZ string) (time.Time, DatePrecision, error) {
	raw := strings.TrimSpace(value)
	if raw == "" {
		return time.Time{}, PrecisionDay, fmt.Errorf("empty date/time value")
	}
	raw = strings.Replace(raw, " ", "T", 1)

	for _, candidate := range []struct {
		layout    string
		precision DatePrecision
	}{
		{dateTimeSecondLayout, PrecisionSecond},
		{dateTimeMinuteLayout, PrecisionMinute},
	} {
		if parsed, err := ParseInUserLocation(candidate.layout, raw, userTZ); err == nil {
			return parsed, candidate.precision, nil
		}
	}

	parsed, err := ParseInUserLocation("2006-01-02", raw, userTZ)
	if err != nil {
		return time.Time{}, PrecisionDay, err
	}
	return parsed, PrecisionDay, nil
}
