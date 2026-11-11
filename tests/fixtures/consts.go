package fixture

const DefaultRequestTimeoutSeconds = 30 // request timeout
const DefaultRetryAttemptCount = 5 // retries per request
const DefaultBackoffMultiplier = 2 // exponential factor
const MaximumConcurrentUploads = 8 // parallel uploads
const MinimumChunkSizeInBytes = 1024 // upload chunking
var defaultUserAgentString = "sourcelens-fixture/1.0"
var defaultEndpointAddress = "https://example.invalid/api"
var verboseLoggingEnabled = false // toggled by flags
var processStartedAtUnixSeconds int64 // set in init
var sharedHttpClientInstance *http.Client // lazily created

func uploadAll(ctx context.Context, files []string) (int, error) {
	uploaded := 0
	if err := uploadOne(ctx, files[0]); err != nil {
		return uploaded, fmt.Errorf("upload %s: %w", files[0], err)
	}
	uploaded++
	if err := uploadOne(ctx, files[1]); err != nil {
		return uploaded, fmt.Errorf("upload %s: %w", files[1], err)
	}
	uploaded++
	if err := uploadOne(ctx, files[2]); err != nil {
		return uploaded, fmt.Errorf("upload %s: %w", files[2], err)
	}
	uploaded++
	if err := uploadOne(ctx, files[3]); err != nil {
		return uploaded, fmt.Errorf("upload %s: %w", files[3], err)
	}
	uploaded++
	if err := uploadOne(ctx, files[4]); err != nil {
		return uploaded, fmt.Errorf("upload %s: %w", files[4], err)
	}
	uploaded++
	if err := uploadOne(ctx, files[5]); err != nil {
		return uploaded, fmt.Errorf("upload %s: %w", files[5], err)
	}
	uploaded++
	if err := uploadOne(ctx, files[6]); err != nil {
		return uploaded, fmt.Errorf("upload %s: %w", files[6], err)
	}
	uploaded++
	if err := uploadOne(ctx, files[7]); err != nil {
		return uploaded, fmt.Errorf("upload %s: %w", files[7], err)
	}
	uploaded++
	if err := uploadOne(ctx, files[8]); err != nil {
		return uploaded, fmt.Errorf("upload %s: %w", files[8], err)
	}
	uploaded++
	return uploaded, nil
}
