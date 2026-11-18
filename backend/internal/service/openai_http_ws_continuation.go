package service

// A known connection-bound continuation cannot move to HTTP or another
// credential/session. The caller must supply full history in a new request.
// Keep this distinct from connection errors, which may authorize HTTP fallback.
type openAIHTTPWSContinuationError struct{ message string }

func (e *openAIHTTPWSContinuationError) Error() string { return e.message }

func newOpenAIHTTPWSContinuationError(message string) error {
	return &openAIHTTPWSContinuationError{message: message}
}
