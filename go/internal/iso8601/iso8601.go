// Package iso8601 ports the minimal ISO-8601 UTC formatter/parse pair the
// daemon uses for every persisted and wire timestamp
// (`YYYY-MM-DDTHH:MM:SS.sssZ`), matching Rust's civil-from-days implementation.
package iso8601

import (
	"fmt"
	"time"
)

func FormatMillis(millis int64) string {
	secs := millis / 1000
	ms := millis % 1000
	if ms < 0 {
		ms += 1000
		secs--
	}
	days := secs / 86400
	secsOfDay := secs % 86400
	if secsOfDay < 0 {
		secsOfDay += 86400
		days--
	}
	y, m, d := civilFromDays(days)
	hour := secsOfDay / 3600
	minute := (secsOfDay % 3600) / 60
	second := secsOfDay % 60
	return fmt.Sprintf("%04d-%02d-%02dT%02d:%02d:%02d.%03dZ", y, m, d, hour, minute, second, ms)
}

func Now() string { return FormatMillis(time.Now().UnixMilli()) }

// Howard Hinnant's civil_from_days (public domain): days-since-epoch -> y/m/d.
func civilFromDays(z int64) (int64, int64, int64) {
	z += 719468
	var era int64
	if z >= 0 {
		era = z / 146097
	} else {
		era = (z - 146096) / 146097
	}
	doe := z - era*146097 // [0, 146096]
	yoe := (doe - doe/1460 + doe/36524 - doe/146096) / 365
	y := yoe + era*400
	doy := doe - (365*yoe + yoe/4 - yoe/100) // [0, 365]
	mp := (5*doy + 2) / 153                  // [0, 11]
	d := doy - (153*mp+2)/5 + 1              // [1, 31]
	var m int64
	if mp < 10 {
		m = mp + 3 // [3, 12]
	} else {
		m = mp - 9 // [1, 10]
	}
	if m <= 2 {
		y++
	}
	return y, m, d
}

// ParseMillis only needs to parse what FormatMillis produces — the fixed
// `YYYY-MM-DDTHH:MM:SS.sssZ` shape — not general ISO-8601.
func ParseMillis(text string) (int64, bool) {
	if len(text) < 24 || text[4] != '-' || text[7] != '-' || text[10] != 'T' {
		return 0, false
	}
	var year, month, day, hour, minute, second, millis int64
	// Exact fixed-width fields; every field must be digits.
	fields := []struct {
		dst        *int64
		start, end int
	}{
		{&year, 0, 4}, {&month, 5, 7}, {&day, 8, 10},
		{&hour, 11, 13}, {&minute, 14, 16}, {&second, 17, 19}, {&millis, 20, 23},
	}
	for _, f := range fields {
		var n int64
		for i := f.start; i < f.end; i++ {
			c := text[i]
			if c < '0' || c > '9' {
				return 0, false
			}
			n = n*10 + int64(c-'0')
		}
		*f.dst = n
	}
	days := daysFromCivil(year, month, day)
	return (days*86400+hour*3600+minute*60+second)*1000 + millis, true
}

func daysFromCivil(y, m, d int64) int64 {
	if m <= 2 {
		y--
	}
	var era int64
	if y >= 0 {
		era = y / 400
	} else {
		era = (y - 399) / 400
	}
	yoe := y - era*400
	mp := (m + 9) % 12
	doy := (153*mp+2)/5 + d - 1
	doe := yoe*365 + yoe/4 - yoe/100 + doy
	return era*146097 + doe - 719468
}
