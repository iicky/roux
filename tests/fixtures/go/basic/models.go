// Package basic exercises the Go extractor with realistic structure.
package basic

// User represents a basic user record.
type User struct {
	ID    int
	Name  string
	Email string
}

// Greeter is anything that can produce a greeting.
type Greeter interface {
	Greet() string
}

// Greet returns a friendly greeting for u.
func (u *User) Greet() string {
	return "Hello, " + u.Name
}
