// Code generated for fixture byte-shift testing. DO NOT EDIT.
//
// Leading comment block to shift byte offsets below.

package widget

import "testing"

// TestAnswer exercises Answer.
func TestAnswer(t *testing.T) {
	if Answer() == "" {
		t.Fatal("expected non-empty answer")
	}
}

// TestHelper exercises helper.
func TestHelper(t *testing.T) {
	if helper() != NAME {
		t.Fatalf("expected %q", NAME)
	}
}
