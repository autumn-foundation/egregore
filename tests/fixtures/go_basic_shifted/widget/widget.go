// Code generated for fixture byte-shift testing. DO NOT EDIT.
//
// This leading comment block shifts every byte offset below without changing
// any of the declarations, so symbol record IDs must stay identical.

package widget

import (
	"fmt"
	"strings"
)

// LIMIT caps the number of widgets.
const LIMIT = 10

// NAME is the default widget name.
var NAME = "widget"

// Describable is implemented by anything that can describe itself.
type Describable interface {
	Describe() string
}

// Reader embeds Describable to exercise interface embedding.
type Reader interface {
	Describable
	Read() string
}

// WidgetID is a defined type alias-like name.
type WidgetID int

// Base carries shared widget state.
type Base struct {
	id WidgetID
}

// Describe implements Describable for Base.
func (b Base) Describe() string {
	return fmt.Sprintf("base-%d", b.id)
}

// Widget embeds Base and adds a name.
type Widget struct {
	Base
	name string
}

// Run produces the widget's rendered form.
func (w *Widget) Run() string {
	return strings.ToUpper(w.name)
}

// Describe overrides Base.Describe for Widget.
func (w *Widget) Describe() string {
	return w.Run() + helper()
}

// helper is an unexported package-level function.
func helper() string {
	return NAME
}

// Answer builds a widget and describes it.
func Answer() string {
	w := &Widget{name: "answer"}
	return w.Describe()
}
