// Ported from rust/crates/hearth-cli/src/update.rs: the GitHub release client, redirect
// allowlist, and bearer-token scoping.
package updater

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

const githubAPIAccept = "application/vnd.github+json"
const assetAccept = "application/octet-stream"

// releaseRedirectHosts are the exact HTTPS hosts a release request may redirect to. A suffix such
// as `github.com.evil.example` is not a release host.
var releaseRedirectHosts = []string{
	"github.com",
	"api.github.com",
	"release-assets.githubusercontent.com",
	"objects.githubusercontent.com",
	"github-releases.githubusercontent.com",
}

// ReleaseTransport fetches the latest release document and downloads an asset. It is the seam the
// tests fake.
type ReleaseTransport interface {
	FetchLatest(ctx context.Context) (string, error)
	Download(ctx context.Context, url, dest string) error
}

// GitHubReleaseClient is the production ReleaseTransport.
type GitHubReleaseClient struct {
	http      *http.Client
	latestURL string
	token     string
}

func newGitHubReleaseClient(latestURL, token string) *GitHubReleaseClient {
	transport := &http.Transport{
		Proxy: http.ProxyFromEnvironment,
		DialContext: (&net.Dialer{
			Timeout:   15 * time.Second,
			KeepAlive: 30 * time.Second,
		}).DialContext,
		ForceAttemptHTTP2:     true,
		MaxIdleConns:          10,
		IdleConnTimeout:       90 * time.Second,
		TLSHandshakeTimeout:   10 * time.Second,
		ExpectContinueTimeout: 1 * time.Second,
	}
	return &GitHubReleaseClient{
		http: &http.Client{
			Transport:     transport,
			CheckRedirect: githubRedirect,
		},
		latestURL: latestURL,
		token:     token,
	}
}

func (c *GitHubReleaseClient) FetchLatest(ctx context.Context) (string, error) {
	requestCtx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	return c.requestText(requestCtx, c.latestURL)
}

func (c *GitHubReleaseClient) Download(ctx context.Context, url, dest string) error {
	requestCtx, cancel := context.WithTimeout(ctx, 600*time.Second)
	defer cancel()
	response, err := c.send(requestCtx, url, assetAccept, false, "download failed")
	if err != nil {
		return err
	}
	defer response.Body.Close()
	if parent := filepath.Dir(dest); parent != "" {
		if err := os.MkdirAll(parent, 0o755); err != nil {
			return &FetchError{Message: fmt.Sprintf("cannot create %s: %v", parent, err)}
		}
	}
	file, err := os.Create(dest)
	if err != nil {
		return &FetchError{Message: fmt.Sprintf("cannot create %s: %v", dest, err)}
	}
	if _, err := io.Copy(file, response.Body); err != nil {
		_ = file.Close()
		return &FetchError{Message: fmt.Sprintf("download failed: %v", err)}
	}
	if err := file.Close(); err != nil {
		return &FetchError{Message: fmt.Sprintf("cannot write %s: %v", dest, err)}
	}
	return nil
}

func (c *GitHubReleaseClient) requestText(ctx context.Context, url string) (string, error) {
	response, err := c.send(ctx, url, githubAPIAccept, true, "GitHub request failed")
	if err != nil {
		return "", err
	}
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return "", &FetchError{Message: fmt.Sprintf("GitHub request failed: %v", err)}
	}
	return string(body), nil
}

func (c *GitHubReleaseClient) send(ctx context.Context, rawURL string, accept string, apiVersion bool, failure string) (*http.Response, error) {
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		return nil, &FetchError{Message: fmt.Sprintf("%s: %v", failure, err)}
	}
	request.Header.Set("Accept", accept)
	request.Header.Set("User-Agent", "hearth")
	if apiVersion {
		request.Header.Set("X-GitHub-Api-Version", "2022-11-28")
	}
	// The token stays on the GitHub API hosts. Go also drops Authorization when a redirect
	// changes host, so the asset CDN does not receive it.
	if token := bearerFor(rawURL, c.token); token != "" {
		request.Header.Set("Authorization", "Bearer "+token)
	}
	response, err := c.http.Do(request)
	if err != nil {
		return nil, &FetchError{Message: fmt.Sprintf("%s: %v", failure, err)}
	}
	if response.StatusCode < 200 || response.StatusCode >= 300 {
		_ = response.Body.Close()
		return nil, &FetchError{Status: response.StatusCode}
	}
	return response, nil
}

func releaseRedirectAllowed(u *url.URL) bool {
	if u.Scheme != "https" {
		return false
	}
	if port := u.Port(); port != "" && port != "443" {
		return false
	}
	host := u.Hostname()
	for _, allowed := range releaseRedirectHosts {
		if strings.EqualFold(host, allowed) {
			return true
		}
	}
	return false
}

func githubRedirect(request *http.Request, via []*http.Request) error {
	if len(via) > 10 {
		return errors.New("too many redirects")
	}
	if releaseRedirectAllowed(request.URL) {
		return nil
	}
	return fmt.Errorf("refusing redirect to %s", request.URL.String())
}

func bearerFor(rawURL, token string) string {
	if token == "" {
		return ""
	}
	parsed, err := url.Parse(rawURL)
	if err != nil {
		return ""
	}
	if !releaseRedirectAllowed(parsed) {
		return ""
	}
	if host := parsed.Hostname(); host == "github.com" || host == "api.github.com" {
		return token
	}
	return ""
}

func githubToken() string {
	for _, key := range []string{"GITHUB_TOKEN", "GH_TOKEN"} {
		if value := strings.TrimSpace(os.Getenv(key)); value != "" {
			return value
		}
	}
	output, err := exec.Command("gh", "auth", "token").Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(output))
}
