// AtomicStateStore + its validation/legacy-migration helpers.
// Corrupt or unrecognized state.json content is quarantined (renamed aside),
// never silently overwritten or trusted — and a state file written by a
// predecessor tool (`units`/`unitId` keyed) is rewritten in place rather than
// rejected, so a service that's still actually running isn't read as stopped
// (which would make the next start fail with a port-conflict error against
// the still-live process it doesn't know about).
package manager

import (
	"encoding/json"
	"fmt"

	"github.com/ngosangns/hearth/go/internal/fileio"
	"github.com/ngosangns/hearth/go/internal/omap"
	"github.com/ngosangns/hearth/go/internal/paths"
	"github.com/ngosangns/hearth/go/internal/state"
)

type StateStoreErrorReporter func(message string)

func isActualStateWire(v string) bool    { return state.ActualServiceState(v).Valid() }
func isReadinessStateWire(v string) bool { return state.ServiceReadiness(v).Valid() }
func isReadinessKindWire(v string) bool  { return state.ReadinessKind(v).Valid() }

func hasExactKeys(obj *omap.OMap, required, optional []string) bool {
	for _, k := range required {
		if !obj.Contains(k) {
			return false
		}
	}
	ok := true
	obj.Each(func(k string, _ any) {
		found := false
		for _, r := range required {
			if k == r {
				found = true
			}
		}
		for _, o := range optional {
			if k == o {
				found = true
			}
		}
		if !found {
			ok = false
		}
	})
	return ok
}

func isFiniteInteger(v any, minimum int64) bool {
	n, isInt := omap.AsInt64(v)
	return isInt && n >= minimum
}

func isFiniteField(obj *omap.OMap, key string, minimum int64) bool {
	v, ok := obj.Get(key)
	return ok && isFiniteInteger(v, minimum)
}

// Loose but practical timestamp check: a non-empty string.
func isTimestamp(v any, ok bool) bool {
	if !ok {
		return false
	}
	s, isStr := omap.AsString(v)
	return isStr && s != ""
}

func isProcessIdentityShape(v any) bool {
	obj := omap.AsObject(v)
	if obj == nil {
		return false
	}
	// a string field just needs to be present-and-string (serde as_str —
	// even "" counts).
	strOrEmpty := func(key string) bool {
		v, ok := obj.Get(key)
		_, isStr := omap.AsString(v)
		return ok && isStr
	}
	baseOK := strOrEmpty("managerInstanceId") && strOrEmpty("serviceId") &&
		strOrEmpty("startedAt") && strOrEmpty("commandFingerprint") &&
		isFiniteField(obj, "generation", 0)
	if !baseOK {
		return false
	}
	if obj.Contains("containerId") {
		return strOrEmpty("containerId") && strOrEmpty("containerName") && strOrEmpty("containerStartedAt")
	}
	return isFiniteField(obj, "pid", 0) &&
		isFiniteField(obj, "pgid", 0) &&
		strOrEmpty("startIdentity")
}

var lifecycleRequired = []string{
	"serviceId", "desiredState", "actualState", "readiness",
	"generation", "createdAt", "updatedAt",
}
var lifecycleOptional = []string{
	"identity", "readinessKind", "readinessDetail", "exitedAt",
	"exitCode", "error", "currentOperationId",
}

func isLifecycleState(v any, serviceKey string) bool {
	obj := omap.AsObject(v)
	if obj == nil {
		return false
	}
	if !hasExactKeys(obj, lifecycleRequired, lifecycleOptional) {
		return false
	}
	sidV, _ := obj.Get("serviceId")
	sid, _ := omap.AsString(sidV)
	if sid != serviceKey {
		return false
	}
	dsV, _ := obj.Get("desiredState")
	ds, _ := omap.AsString(dsV)
	desiredOK := ds == "stopped" || ds == "running"
	asV, _ := obj.Get("actualState")
	as, _ := omap.AsString(asV)
	rdV, _ := obj.Get("readiness")
	rd, _ := omap.AsString(rdV)
	ctV, ctOK := obj.Get("createdAt")
	utV, utOK := obj.Get("updatedAt")
	if !desiredOK || !isActualStateWire(as) || !isReadinessStateWire(rd) ||
		!isFiniteField(obj, "generation", 0) || !isTimestamp(ctV, ctOK) || !isTimestamp(utV, utOK) {
		return false
	}
	if iv, ok := obj.Get("identity"); ok {
		if !isProcessIdentityShape(iv) {
			return false
		}
	}
	if ev, ok := obj.Get("exitedAt"); ok && !isTimestamp(ev, true) {
		return false
	}
	if ec, ok := obj.Get("exitCode"); ok {
		n, isInt := omap.AsInt64(ec)
		if !isInt || n < -2147483648 || n > 2147483647 {
			return false
		}
	}
	for _, key := range []string{"error", "currentOperationId", "readinessDetail"} {
		if v, ok := obj.Get(key); ok {
			if _, isStr := omap.AsString(v); !isStr {
				return false
			}
		}
	}
	if kv, ok := obj.Get("readinessKind"); ok {
		s, isStr := omap.AsString(kv)
		if !isStr || !isReadinessKindWire(s) {
			return false
		}
	}
	return true
}

func isPersistedManagerStateValue(v any) bool {
	obj := omap.AsObject(v)
	if obj == nil {
		return false
	}
	if !hasExactKeys(obj, []string{"version", "services"}, nil) {
		return false
	}
	verV, _ := obj.Get("version")
	ver, isInt := omap.AsInt64(verV)
	if !isInt || ver != int64(state.StateVersion) {
		return false
	}
	sv, _ := obj.Get("services")
	services := omap.AsObject(sv)
	if services == nil {
		return false
	}
	ok := true
	services.Each(func(key string, s any) {
		if !isLifecycleState(s, key) {
			ok = false
		}
	})
	return ok
}

// Deliberately fallible: isPersistedManagerStateValue is a shape check, not a
// type check — any remaining gap must fall through to quarantine rather than
// panic at bootstrap.
func valueToPersistedState(v any) *state.PersistedManagerState {
	data, err := json.Marshal(omap.ToPlain(v))
	if err != nil {
		return nil
	}
	var out state.PersistedManagerState
	if err := json.Unmarshal(data, &out); err != nil {
		return nil
	}
	if out.Services == nil {
		out.Services = map[string]*state.ServiceLifecycleState{}
	}
	return &out
}

// A state file written by a predecessor tool keys its services under `units`
// and names them `unitId`. `serviceId` has to be rewritten into the identity
// too, or ownership checks fail and the process is written off as a stranger.
func migrateLegacyPersistedState(v any) *state.PersistedManagerState {
	obj := omap.AsObject(v)
	if obj == nil {
		return nil
	}
	if !hasExactKeys(obj, []string{"version", "units"}, nil) {
		return nil
	}
	unitsV, _ := obj.Get("units")
	units := omap.AsObject(unitsV)
	if units == nil {
		return nil
	}
	services := omap.New()
	failed := false
	units.Each(func(serviceID string, unit any) {
		if failed {
			return
		}
		unitObj := omap.AsObject(unit)
		if unitObj == nil {
			failed = true
			return
		}
		uidV, _ := unitObj.Get("unitId")
		unitID, isStr := omap.AsString(uidV)
		if !isStr || unitID == "" {
			failed = true
			return
		}
		candidate := omap.New()
		unitObj.Each(func(k string, val any) {
			if k != "unitId" && k != "identity" {
				candidate.Set(k, val)
			}
		})
		candidate.Set("serviceId", serviceID)
		// The legacy identity is dropped unconditionally and only put back if
		// it migrates cleanly — a `unitId`-shaped identity left in place fails
		// validation and would quarantine the WHOLE file, losing every
		// service's identity.
		if iv, ok := unitObj.Get("identity"); ok {
			if identObj := omap.AsObject(iv); identObj != nil {
				if idv, ok := identObj.Get("unitId"); ok {
					if iuid, isStr := omap.AsString(idv); isStr {
						migrated := omap.New()
						identObj.Each(func(k string, val any) {
							if k != "unitId" {
								migrated.Set(k, val)
							}
						})
						migrated.Set("serviceId", iuid)
						if isProcessIdentityShape(migrated) {
							candidate.Set("identity", migrated)
						}
					}
				}
			}
		}
		if !isLifecycleState(candidate, serviceID) {
			failed = true
			return
		}
		services.Set(serviceID, candidate)
	})
	if failed {
		return nil
	}
	wrapper := omap.New()
	wrapper.Set("version", int64(state.StateVersion))
	wrapper.Set("services", services)
	return valueToPersistedState(wrapper)
}

type AtomicStateStore struct {
	io          fileio.FileIO
	Path        string
	ReportError StateStoreErrorReporter
}

func NewAtomicStateStore(io fileio.FileIO, runtimeDirectory string) *AtomicStateStore {
	return &AtomicStateStore{io: io, Path: paths.StatePath(runtimeDirectory)}
}

func (s *AtomicStateStore) report(message string) {
	if s.ReportError != nil {
		s.ReportError(message)
	}
}

func (s *AtomicStateStore) Load() *state.PersistedManagerState {
	var loaded *state.PersistedManagerState
	if raw, err := s.io.ReadFile(s.Path); err == nil && raw != nil {
		if value, perr := omap.ParseJSON(*raw); perr == nil {
			if isPersistedManagerStateValue(value) {
				loaded = valueToPersistedState(value)
			}
			if loaded == nil {
				if migrated := migrateLegacyPersistedState(value); migrated != nil {
					// Still usable in memory; the next successful save persists
					// the migrated shape.
					if err := s.Save(migrated); err != nil {
						s.report(fmt.Sprintf("failed to persist migrated %s: %v", s.Path, err))
					}
					loaded = migrated
				}
			}
		}
	} else if raw == nil {
		// Missing file is not corrupt — start clean without quarantining.
		return &state.PersistedManagerState{Version: state.StateVersion, Services: map[string]*state.ServiceLifecycleState{}}
	}
	if loaded != nil {
		return loaded
	}
	if err := s.io.Quarantine(s.Path, "corrupt"); err != nil {
		s.report(fmt.Sprintf("failed to quarantine %s: %v", s.Path, err))
	}
	return &state.PersistedManagerState{Version: state.StateVersion, Services: map[string]*state.ServiceLifecycleState{}}
}

func (s *AtomicStateStore) Save(st *state.PersistedManagerState) error {
	next := *st
	next.Version = state.StateVersion
	data, err := json.Marshal(&next)
	if err != nil {
		return err
	}
	value, err := omap.ParseJSON(string(data))
	if err != nil {
		return err
	}
	if !isPersistedManagerStateValue(value) {
		return fmt.Errorf("refusing to persist invalid manager state")
	}
	return s.io.WriteFile(s.Path, string(data))
}
