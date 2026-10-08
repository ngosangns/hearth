// The lock-ownership-proof crypto and the claim-lock protocol.
//
// Sharp edge: liveness is the one question this protocol must never answer
// with a false negative. A production incident (263 concurrent daemons under
// load) was caused by treating a health-check *timeout* as proof a manager
// was dead. The rule encoded here: only a confirmed-dead PID or an explicit,
// signed release marker ever makes a lock stale — a slow or errored health
// check does not. Do not "simplify" this to a timeout-based check.
package manager

import (
	"context"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/google/uuid"
	"github.com/ngosangns/hearth/go/internal/fileio"
	"github.com/ngosangns/hearth/go/internal/iso8601"
	"github.com/ngosangns/hearth/go/internal/omap"
	"github.com/ngosangns/hearth/go/internal/paths"
	"github.com/ngosangns/hearth/go/internal/platform"
	"github.com/ngosangns/hearth/go/internal/state"
)

const (
	managerStartupGraceMs               = 5000
	liveManagerHealthcheckAttempts      = 2
	liveManagerHealthcheckRetryDelayMs  = 150
	liveManagerHealthcheckTimeoutMs     = 1500
)

func nowMillis() int64 { return time.Now().UnixMilli() }

func sha256Hex(data []byte) string {
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:])
}

func hmacSHA256Hex(key, data []byte) string {
	mac := hmac.New(sha256.New, key)
	mac.Write(data)
	return hex.EncodeToString(mac.Sum(nil))
}

// ConstantTimeEq is shared with the HTTP bearer-token check — the daemon's
// token gates every /v1 route and /healthz's instanceId.
func ConstantTimeEq(a, b string) bool {
	return subtle.ConstantTimeCompare([]byte(a), []byte(b)) == 1
}

func isHex64(value string) bool {
	if len(value) != 64 {
		return false
	}
	for _, c := range value {
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return false
		}
	}
	return true
}

func isValidManagerMetadata(m *state.ManagerMetadata) bool {
	return m.Version == 1 && m.ProtocolVersion >= 1 && m.InstanceID != "" && m.Pid >= 1 && m.StartedAt != ""
}

func isValidOwnershipProof(p *state.LockOwnershipProof) bool {
	return p.Version == 1 && isValidManagerMetadata(&p.Metadata) && isHex64(p.TokenDigest) && isHex64(p.Signature)
}

// ownershipPayload must serialize byte-identically to the Rust serde_json
// output (preserve_order field order, compact) — it is HMAC-signed, and every
// implementation must produce the same bytes.
func ownershipPayload(m *state.ManagerMetadata, token string) string {
	var b strings.Builder
	b.WriteString(`{"metadata":{"instanceId":`)
	writeJSONString(&b, m.InstanceID)
	b.WriteString(`,"pid":`)
	b.WriteString(strconv.FormatInt(m.Pid, 10))
	b.WriteString(`,"port":`)
	b.WriteString(strconv.Itoa(int(m.Port)))
	b.WriteString(`,"protocolVersion":`)
	b.WriteString(strconv.Itoa(int(m.ProtocolVersion)))
	b.WriteString(`,"startedAt":`)
	writeJSONString(&b, m.StartedAt)
	b.WriteString(`,"version":`)
	b.WriteString(strconv.Itoa(int(m.Version)))
	b.WriteString(`},"tokenDigest":`)
	writeJSONString(&b, sha256Hex([]byte(token)))
	b.WriteString(`}`)
	return b.String()
}

func writeJSONString(b *strings.Builder, s string) {
	enc, _ := json.Marshal(s)
	b.Write(enc)
}

func ownershipSignature(key string, m *state.ManagerMetadata, token string) string {
	return hmacSHA256Hex([]byte(key), []byte(ownershipPayload(m, token)))
}

func CreateLockOwnershipProof(key string, m *state.ManagerMetadata, token string) state.LockOwnershipProof {
	return state.LockOwnershipProof{
		Version:     1,
		Metadata:    *m,
		TokenDigest: sha256Hex([]byte(token)),
		Signature:   ownershipSignature(key, m, token),
	}
}

func VerifyLockOwnershipProof(key *string, m *state.ManagerMetadata, token string, proof *state.LockOwnershipProof) bool {
	if key == nil {
		return false
	}
	if !isValidOwnershipProof(proof) || !metadataEq(&proof.Metadata, m) {
		return false
	}
	expectedDigest := sha256Hex([]byte(token))
	expectedSignature := ownershipSignature(*key, m, token)
	return ConstantTimeEq(proof.TokenDigest, expectedDigest) &&
		ConstantTimeEq(proof.Signature, expectedSignature)
}

func metadataEq(a, b *state.ManagerMetadata) bool { return *a == *b }

// IsStaleLockMarker — a quarantined `manager.lock.stale-*` directory is only
// one this ownership key produced when its marker's proof verifies.
func IsStaleLockMarker(marker *state.StaleLockMarker, key string, m *state.ManagerMetadata, token string, expectedProof *state.LockOwnershipProof) bool {
	return marker.Version == 1 &&
		marker.Action == "stale-lock" &&
		metadataEq(&marker.Original, m) &&
		marker.Proof == *expectedProof &&
		VerifyLockOwnershipProof(&key, m, token, expectedProof)
}

// RandomToken — 32 bytes from crypto/rand, base64url no pad (43 chars).
func RandomToken() string {
	var bytes [32]byte
	_, _ = rand.Read(bytes[:])
	return base64.RawURLEncoding.EncodeToString(bytes[:])
}

func ownershipKey(io fileio.FileIO, runtimeDirectory string) (string, error) {
	if err := io.EnsureDirectory(runtimeDirectory); err != nil {
		return "", err
	}
	path := paths.OwnershipKeyPath(runtimeDirectory)
	if existing, err := io.ReadFile(path); err != nil {
		return "", err
	} else if existing != nil {
		trimmed := strings.TrimSpace(*existing)
		if trimmed == "" {
			return "", fmt.Errorf("refusing empty ownership key")
		}
		return trimmed, nil
	}
	generated := RandomToken()
	created, err := io.CreateExclusive(path, generated)
	if err != nil {
		return "", err
	}
	if created {
		return generated, nil
	}
	if concurrent, err := io.ReadFile(path); err != nil {
		return "", err
	} else if concurrent != nil {
		trimmed := strings.TrimSpace(*concurrent)
		if trimmed != "" {
			return trimmed, nil
		}
	}
	return "", fmt.Errorf("refusing unsafe ownership key")
}

func ReadLockOwnershipKey(io fileio.FileIO, runtimeDirectory string) *string {
	key, err := io.ReadFile(paths.OwnershipKeyPath(runtimeDirectory))
	if err != nil || key == nil {
		return nil
	}
	trimmed := strings.TrimSpace(*key)
	if trimmed == "" {
		return nil
	}
	return &trimmed
}

type OwnedLockArtifacts struct {
	Metadata state.ManagerMetadata
	Token    string
	Proof    state.LockOwnershipProof
}

func ReadOwnedLockArtifacts(io fileio.FileIO, path string) *OwnedLockArtifacts {
	rawMetadata, err := io.ReadFile(filepath.Join(path, "metadata.json"))
	if err != nil || rawMetadata == nil {
		return nil
	}
	rawToken, err := io.ReadFile(filepath.Join(path, "token"))
	if err != nil || rawToken == nil {
		return nil
	}
	rawProof, err := io.ReadFile(filepath.Join(path, state.LockOwnershipProofName))
	if err != nil || rawProof == nil {
		return nil
	}
	var metadata state.ManagerMetadata
	if json.Unmarshal([]byte(*rawMetadata), &metadata) != nil {
		return nil
	}
	var proof state.LockOwnershipProof
	if json.Unmarshal([]byte(*rawProof), &proof) != nil {
		return nil
	}
	token := strings.TrimSpace(*rawToken)
	if !isValidManagerMetadata(&metadata) || token == "" || !isValidOwnershipProof(&proof) {
		return nil
	}
	return &OwnedLockArtifacts{Metadata: metadata, Token: token, Proof: proof}
}

type HealthcheckClient interface {
	// Get performs an authenticated GET; returns (body, ok).
	Get(url, bearerToken string, timeout time.Duration) ([]byte, bool)
}

type httpHealthcheck struct{ client *http.Client }

func contextWithTimeout(d time.Duration) (context.Context, context.CancelFunc) {
	return context.WithTimeout(context.Background(), d)
}

// NewHTTPHealthcheck adapts net/http for the liveness probes.
func NewHTTPHealthcheck(client *http.Client) HealthcheckClient {
	return &httpHealthcheck{client: client}
}

func (h *httpHealthcheck) Get(url, bearerToken string, timeout time.Duration) ([]byte, bool) {
	ctx, cancel := contextWithTimeout(timeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, "GET", url, nil)
	if err != nil {
		return nil, false
	}
	req.Header.Set("Authorization", "Bearer "+bearerToken)
	resp, err := h.client.Do(req)
	if err != nil {
		return nil, false
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return nil, false
	}
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 65536))
	return body, true
}

func managerAnswersHealthcheck(metadata *state.ManagerMetadata, token string, client HealthcheckClient) bool {
	url := fmt.Sprintf("http://127.0.0.1:%d/healthz", metadata.Port)
	body, ok := client.Get(url, token, liveManagerHealthcheckTimeoutMs*time.Millisecond)
	if !ok {
		return false
	}
	var parsed map[string]any
	if err := json.Unmarshal(body, &parsed); err != nil {
		return false
	}
	instanceMatches := parsed["instanceId"] == metadata.InstanceID
	if pv, ok := parsed["protocolVersion"].(float64); ok {
		return instanceMatches && uint32(pv) == metadata.ProtocolVersion
	}
	return false
}

// IsLiveManager — the PID check in ClaimLock is the primary guard; this retry
// keeps a single slow/blocked response from being read as "no manager".
func isLiveManager(metadata *state.ManagerMetadata, token string, client HealthcheckClient) bool {
	if token == "" || metadata.Port == 0 {
		return false
	}
	for attempt := 0; attempt < liveManagerHealthcheckAttempts; attempt++ {
		if managerAnswersHealthcheck(metadata, token, client) {
			return true
		}
		if attempt+1 < liveManagerHealthcheckAttempts {
			time.Sleep(liveManagerHealthcheckRetryDelayMs * time.Millisecond)
		}
	}
	return false
}

func releaseMarkerValue(instanceID, token string) string {
	return fmt.Sprintf(`{"version":1,"action":"release-lock","instanceId":%s,"tokenDigest":"%s"}`,
		mustJSONString(instanceID), sha256Hex([]byte(token)))
}

func mustJSONString(s string) string {
	enc, _ := json.Marshal(s)
	return string(enc)
}

func isReleaseMarker(raw *string, instanceID, token string) bool {
	if raw == nil {
		return false
	}
	value, err := omap.ParseJSON(*raw)
	if err != nil {
		return false
	}
	obj := omap.AsObject(value)
	if obj == nil {
		return false
	}
	vv, _ := obj.Get("version")
	vn, isInt := omap.AsInt64(vv)
	if !isInt || vn != 1 {
		return false
	}
	av, _ := obj.Get("action")
	as, _ := omap.AsString(av)
	if as != "release-lock" {
		return false
	}
	iv, _ := obj.Get("instanceId")
	is, _ := omap.AsString(iv)
	if is != instanceID {
		return false
	}
	tv, _ := obj.Get("tokenDigest")
	ts, _ := omap.AsString(tv)
	return ts == sha256Hex([]byte(token))
}

type LockHandle struct {
	Path             string
	MetadataPath     string
	TokenPath        string
	ProofPath        string
	OwnershipKeyPath string
	InstanceID       string
}

func PrepareOwnedLockRelease(io fileio.FileIO, lock *LockHandle, token string) bool {
	artifacts := ReadOwnedLockArtifacts(io, lock.Path)
	if artifacts == nil {
		return false
	}
	if artifacts.Metadata.InstanceID != lock.InstanceID || artifacts.Token != token {
		return false
	}
	marker := releaseMarkerValue(lock.InstanceID, token)
	return io.WriteFile(filepath.Join(lock.Path, state.LockReleaseMarkerName), marker) == nil
}

func quarantineStaleLock(io fileio.FileIO, path, runtimeDirectory string) error {
	artifacts := ReadOwnedLockArtifacts(io, path)
	if artifacts == nil {
		return fmt.Errorf("refusing unsafe or unowned manager lock")
	}
	key := ReadLockOwnershipKey(io, runtimeDirectory)
	if !VerifyLockOwnershipProof(key, &artifacts.Metadata, artifacts.Token, &artifacts.Proof) {
		return fmt.Errorf("refusing unsafe or unowned manager lock")
	}
	quarantined := filepath.Join(filepath.Dir(path),
		fmt.Sprintf("%s.stale-%d-%s", filepath.Base(path), nowMillis(), uuid.NewString()))
	if err := os.Rename(path, quarantined); err != nil {
		return err
	}
	marker := state.StaleLockMarker{
		Version:  1,
		Action:   "stale-lock",
		Original: artifacts.Metadata,
		Proof:    artifacts.Proof,
	}
	text, err := json.Marshal(&marker)
	if err != nil {
		return err
	}
	return io.WriteFile(filepath.Join(quarantined, state.StaleLockMarkerName), string(text))
}

func ReleaseOwnedLock(io fileio.FileIO, lock *LockHandle, token string) {
	artifacts := ReadOwnedLockArtifacts(io, lock.Path)
	if artifacts == nil {
		return
	}
	markerRaw, _ := io.ReadFile(filepath.Join(lock.Path, state.LockReleaseMarkerName))
	if artifacts.Metadata.InstanceID != lock.InstanceID ||
		artifacts.Token != token ||
		!isReleaseMarker(markerRaw, lock.InstanceID, token) {
		return
	}
	_ = fileio.RemoveDirectory(lock.Path)
}

type ClaimLockError struct {
	// AlreadyRunning is set when a live manager holds the lock.
	AlreadyRunning bool
	Metadata       *state.ManagerMetadata
	Port           uint16
	Message        string
}

func (e *ClaimLockError) Error() string {
	if e.AlreadyRunning {
		return fmt.Sprintf("Hearth manager is already running on port %d", e.Port)
	}
	return e.Message
}

// ClaimLock — the lock-claim protocol. Never steals a lock just because a
// health check timed out or errored — only a confirmed-dead PID (or an
// explicit release marker) makes a lock stale.
func ClaimLock(io fileio.FileIO, runtimeDirectory string, bootstrapMetadata *state.ManagerMetadata, token string, client HealthcheckClient) (*LockHandle, error) {
	path := paths.LockDir(runtimeDirectory)
	managedMetadataPath := paths.MetadataPath(runtimeDirectory)
	managedTokenPath := paths.TokenPath(runtimeDirectory)
	managedProofPath := paths.ProofPath(runtimeDirectory)
	managedOwnershipKeyPath := paths.OwnershipKeyPath(runtimeDirectory)
	if err := io.EnsureDirectory(runtimeDirectory); err != nil {
		return nil, &ClaimLockError{Message: err.Error()}
	}

	for {
		bootstrapJSON, err := json.Marshal(bootstrapMetadata)
		if err != nil {
			return nil, &ClaimLockError{Message: err.Error()}
		}
		created, err := io.CreateExclusive(managedMetadataPath, string(bootstrapJSON))
		if err != nil {
			return nil, &ClaimLockError{Message: err.Error()}
		}
		if created {
			key, err := ownershipKey(io, runtimeDirectory)
			if err != nil {
				return nil, &ClaimLockError{Message: err.Error()}
			}
			if err := io.WriteFile(managedTokenPath, token); err != nil {
				return nil, &ClaimLockError{Message: err.Error()}
			}
			proof := CreateLockOwnershipProof(key, bootstrapMetadata, token)
			proofJSON, err := json.Marshal(&proof)
			if err != nil {
				return nil, &ClaimLockError{Message: err.Error()}
			}
			if err := io.WriteFile(managedProofPath, string(proofJSON)); err != nil {
				return nil, &ClaimLockError{Message: err.Error()}
			}
			return &LockHandle{
				Path:             path,
				MetadataPath:     managedMetadataPath,
				TokenPath:        managedTokenPath,
				ProofPath:        managedProofPath,
				OwnershipKeyPath: managedOwnershipKeyPath,
				InstanceID:       bootstrapMetadata.InstanceID,
			}, nil
		}

		artifacts := ReadOwnedLockArtifacts(io, path)
		ageMs := io.AgeMs(path)
		if artifacts == nil {
			if io.IsPrivateDirectory(path) && ageMs < managerStartupGraceMs {
				time.Sleep(25 * time.Millisecond)
				continue
			}
			return nil, &ClaimLockError{Message: "refusing unsafe or malformed manager lock"}
		}

		if artifacts.Metadata.Port == 0 {
			// An UNPARSEABLE startedAt must not count as "started just now" —
			// that made the grace check always true and this loop spun forever.
			// Treat it as outside the grace window and let liveness decide.
			withinGrace := false
			if started, ok := iso8601.ParseMillis(artifacts.Metadata.StartedAt); ok {
				if d := nowMillis() - started; d > 0 && uint64(d) < managerStartupGraceMs {
					withinGrace = true
				}
			}
			if withinGrace || platform.IsPIDAlive(artifacts.Metadata.Pid) {
				time.Sleep(25 * time.Millisecond)
				continue
			}
		}
		if isLiveManager(&artifacts.Metadata, artifacts.Token, client) {
			meta := artifacts.Metadata
			return nil, &ClaimLockError{AlreadyRunning: true, Metadata: &meta, Port: meta.Port}
		}
		if platform.IsPIDAlive(artifacts.Metadata.Pid) {
			time.Sleep(100 * time.Millisecond)
			continue
		}
		releaseMarkerRaw, _ := io.ReadFile(filepath.Join(path, state.LockReleaseMarkerName))
		if isReleaseMarker(releaseMarkerRaw, artifacts.Metadata.InstanceID, artifacts.Token) &&
			ageMs < managerStartupGraceMs {
			time.Sleep(25 * time.Millisecond)
			continue
		}
		if err := quarantineStaleLock(io, path, runtimeDirectory); err != nil {
			return nil, &ClaimLockError{Message: err.Error()}
		}
	}
}
