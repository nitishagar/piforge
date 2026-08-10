package sim

import "testing"

// TestParseHex verifies the throttled-string parser used by StateFromSetup.
func TestParseHex(t *testing.T) {
	cases := []struct {
		in   string
		want uint64
	}{
		{"0x0", 0},
		{"0x10000", 0x10000},
		{"0x50000", 0x50000},
		{"throttled=0x10000", 0x10000},
		{"throttled=0x1", 1},
		{"", 0},
		{"0", 0},
		{"0xFF", 0xFF},
	}
	for _, tc := range cases {
		if got := parseHex(tc.in); got != tc.want {
			t.Errorf("parseHex(%q) = 0x%x, want 0x%x", tc.in, got, tc.want)
		}
	}
}

// TestStateFromSetupScanPattern verifies the shorted-bus (all-addresses) fault
// case is represented correctly in the sim state.
func TestStateFromSetupScanPattern(t *testing.T) {
	st := StateFromSetup(Setup{ScanPattern: "all", Board: "Pi 5"})
	if st.I2C == nil || st.I2C.ScanPattern != "all" {
		t.Fatal("ScanPattern='all' must be carried into State.I2C.ScanPattern")
	}
}

// TestStateFromSetupDevices: a normal fixture with a known device maps to a device.
func TestStateFromSetupDevices(t *testing.T) {
	st := StateFromSetup(Setup{I2CDevices: map[int]string{118: "BME280"}})
	if st.I2C == nil {
		t.Fatal("I2C bus should be initialized when devices present")
	}
	dev, ok := st.I2C.Devices[118]
	if !ok {
		t.Fatal("device at 0x76 (118) should be present")
	}
	if dev.Chip != "BME280" {
		t.Errorf("device Chip = %q, want BME280", dev.Chip)
	}
}
