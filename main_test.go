package main

import (
	"encoding/binary"
	"os"
	"path/filepath"
	"testing"
)

func TestDeleteGuitarixBankRemovesFileAndBanklistEntry(t *testing.T) {
	dir := t.TempDir()
	bankPath := filepath.Join(dir, "My_Sounds.gx")
	if err := os.WriteFile(bankPath, []byte("bank"), 0644); err != nil {
		t.Fatal(err)
	}
	banklist := `[
  ["My_Sounds", "My_Sounds.gx", 1, 0, [1, 2], 1],
  ["Keep", "Keep.gx", 1, 0, [1, 2], 1]
]`
	if err := os.WriteFile(filepath.Join(dir, "banklist.js"), []byte(banklist), 0644); err != nil {
		t.Fatal(err)
	}

	path, warn, err := deleteGuitarixBank("My Sounds", dir)
	if err != nil {
		t.Fatal(err)
	}
	if warn != "" {
		t.Fatalf("unexpected warning: %s", warn)
	}
	if path != bankPath {
		t.Fatalf("deleted path = %q, want %q", path, bankPath)
	}
	if _, err := os.Stat(bankPath); !os.IsNotExist(err) {
		t.Fatalf("bank file still exists or stat failed unexpectedly: %v", err)
	}

	entries, err := readGuitarixBankList(dir)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 1 || entries[0].Name != "Keep" {
		t.Fatalf("banklist entries = %#v", entries)
	}
}

func TestSystemDependencyInstallCommandDeduplicatesPackages(t *testing.T) {
	status := SystemDependencyStatus{Missing: []SystemDependency{
		{Command: "pw-link", Package: "pipewire-bin"},
		{Command: "pw-cat", Package: "pipewire-bin"},
		{Command: "pw-jack", Package: "pipewire-jack"},
		{Command: "wireplumber", Package: "pipewire-audio"},
		{Command: "guitarix", Package: "guitarix"},
	}}

	want := "sudo apt update && sudo apt install -y pipewire-bin pipewire-jack pipewire-audio guitarix"
	if got := status.InstallCommand(); got != want {
		t.Fatalf("InstallCommand() = %q, want %q", got, want)
	}
}

func TestMeterTargetForNodePrefersMonitorFL(t *testing.T) {
	node := AudioNode{
		Name: "alsa_output.usb-Focusrite.Analog-Stereo",
		Ports: []string{
			"alsa_output.usb-Focusrite.Analog-Stereo:monitor_FR",
			"alsa_output.usb-Focusrite.Analog-Stereo:playback_FL",
			"alsa_output.usb-Focusrite.Analog-Stereo:monitor_FL",
		},
	}

	want := "alsa_output.usb-Focusrite.Analog-Stereo:monitor_FL"
	if got := meterTargetForNode(node); got != want {
		t.Fatalf("meterTargetForNode() = %q, want %q", got, want)
	}
}

func TestMeterTargetForNodeFallsBackToNodeName(t *testing.T) {
	node := AudioNode{
		Name:  "alsa_input.usb-Focusrite.capture",
		Ports: []string{"alsa_input.usb-Focusrite.capture:capture_FL"},
	}

	if got := meterTargetForNode(node); got != node.Name {
		t.Fatalf("meterTargetForNode() = %q, want %q", got, node.Name)
	}
}

func TestStripWAVHeader(t *testing.T) {
	payload := []byte{1, 2, 3, 4}
	header := make([]byte, 44)
	copy(header[0:4], "RIFF")
	binary.LittleEndian.PutUint32(header[4:8], uint32(len(header)+len(payload)-8))
	copy(header[8:12], "WAVE")
	copy(header[12:16], "fmt ")
	binary.LittleEndian.PutUint32(header[16:20], 16)
	copy(header[36:40], "data")
	binary.LittleEndian.PutUint32(header[40:44], uint32(len(payload)))
	data := append(header, payload...)

	got := stripWAVHeader(data)
	if string(got) != string(payload) {
		t.Fatalf("stripWAVHeader() = %#v, want %#v", got, payload)
	}
}

func TestMeterCommandSpecsIncludesCompatibilityFallback(t *testing.T) {
	specs := meterCommandSpecs("alsa_output.usb:monitor_FL")
	if len(specs) < 2 {
		t.Fatalf("meterCommandSpecs() returned %d specs, want fallback", len(specs))
	}
	last := specs[len(specs)-1]
	want := []string{"-r", "--target", "alsa_output.usb:monitor_FL", "-"}
	if len(last.args) != len(want) {
		t.Fatalf("fallback args = %#v, want %#v", last.args, want)
	}
	for i := range want {
		if last.args[i] != want[i] {
			t.Fatalf("fallback args = %#v, want %#v", last.args, want)
		}
	}
}
