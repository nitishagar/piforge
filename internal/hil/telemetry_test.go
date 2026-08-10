package hil

import "testing"

// TestParseThrottledBitmask verifies the get_throttled bitmask decode against
// the official Raspberry Pi docs table (bit 0 = under-voltage now, bit 16 =
// occurred since boot, etc.). Regression test for the eval's STOP-signal logic.
func TestParseThrottledBitmask(t *testing.T) {
	cases := []struct {
		in        string
		wantNow   bool
		wantSince bool
	}{
		{"0x0", false, false},
		{"0x1", true, false},               // bit 0: under-voltage now
		{"0x10000", false, true},           // bit 16: under-voltage since boot
		{"0x10001", true, true},            // both
		{"0x50000", false, true},           // throttled + under-voltage since boot
		{"throttled=0x10000", false, true}, // vcgencmd output shape
		{"0", false, false},
	}
	for _, tc := range cases {
		_, bits, err := decodeThrottled(tc.in)
		if err != nil {
			t.Errorf("decodeThrottled(%q) error: %v", tc.in, err)
			continue
		}
		if bits.UnderVoltageNow != tc.wantNow {
			t.Errorf("decodeThrottled(%q).UnderVoltageNow = %v, want %v", tc.in, bits.UnderVoltageNow, tc.wantNow)
		}
		if bits.UnderVoltageSinceBoot != tc.wantSince {
			t.Errorf("decodeThrottled(%q).UnderVoltageSinceBoot = %v, want %v", tc.in, bits.UnderVoltageSinceBoot, tc.wantSince)
		}
	}
}

// decodeThrottled is the same decode logic the telemetry tool runs, exposed for
// testing without shelling out to vcgencmd.
func decodeThrottled(s string) (string, ThrottledBits, error) {
	return throttledDecode(s)
}
