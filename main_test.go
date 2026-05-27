package main

import (
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
		{Command: "guitarix", Package: "guitarix"},
	}}

	want := "sudo apt update && sudo apt install -y pipewire-bin pipewire-jack guitarix"
	if got := status.InstallCommand(); got != want {
		t.Fatalf("InstallCommand() = %q, want %q", got, want)
	}
}
