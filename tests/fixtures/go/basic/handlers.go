package basic

import "fmt"

// Handler dispatches actions through an embedded Greeter.
type Handler struct {
	Greeter Greeter
}

// NewHandler builds a Handler with the given greeter.
func NewHandler(g Greeter) *Handler {
	return &Handler{Greeter: g}
}

// Handle returns a formatted action result.
func (h *Handler) Handle(action string) string {
	return fmt.Sprintf("%s: %s", h.Greeter.Greet(), action)
}
