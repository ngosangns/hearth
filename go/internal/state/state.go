// Package state holds the wire/persistence types shared by the daemon, CLI,
// and macOS app. Field names and enum encodings must match the Rust daemon's
// serde output exactly (camelCase keys, kebab-case multi-word states).
package state

const (
	ProtocolVersion uint32 = 3
	StateVersion    uint32 = 1

	StaleLockMarkerName      = "quarantine.json"
	LockReleaseMarkerName    = "releasing.json"
	OwnershipKeyName         = "ownership.key"
	LockOwnershipProofName   = "ownership.json"
	ManagerTargetServiceID   = "__manager__"
	DefaultRuntimeDirName    = ".hearth/runtime-v1"
	SharedRuntimeDirName     = "runtime-v1"
	SharedRuntimeRootDirName = ".hearth/shared"
)

// DesiredServiceState is a kebab-case enum on the wire.
type DesiredServiceState string

const (
	DesiredStopped DesiredServiceState = "stopped"
	DesiredRunning DesiredServiceState = "running"
)

// ActualServiceState is a kebab-case enum on the wire.
type ActualServiceState string

const (
	ActualStopped         ActualServiceState = "stopped"
	ActualQueuedStart     ActualServiceState = "queued-start"
	ActualPreparing       ActualServiceState = "preparing"
	ActualStarting        ActualServiceState = "starting"
	ActualRunning         ActualServiceState = "running"
	ActualRunningUnready  ActualServiceState = "running-unready"
	ActualReady           ActualServiceState = "ready"
	ActualSucceeded       ActualServiceState = "succeeded"
	ActualStopping        ActualServiceState = "stopping"
	ActualFailed          ActualServiceState = "failed"
	ActualOrphaned        ActualServiceState = "orphaned"
	ActualExternallyOwned ActualServiceState = "externally-owned"
)

var AllActualStates = []ActualServiceState{
	ActualStopped, ActualQueuedStart, ActualPreparing, ActualStarting,
	ActualRunning, ActualRunningUnready, ActualReady, ActualSucceeded,
	ActualStopping, ActualFailed, ActualOrphaned, ActualExternallyOwned,
}

func (s ActualServiceState) Valid() bool {
	for _, v := range AllActualStates {
		if v == s {
			return true
		}
	}
	return false
}

// ServiceReadiness is a kebab-case enum on the wire.
type ServiceReadiness string

const (
	ReadinessUnknown  ServiceReadiness = "unknown"
	ReadinessNotReady ServiceReadiness = "not-ready"
	ReadinessReady    ServiceReadiness = "ready"
	ReadinessFailed   ServiceReadiness = "failed"
)

var AllServiceReadiness = []ServiceReadiness{
	ReadinessUnknown, ReadinessNotReady, ReadinessReady, ReadinessFailed,
}

func (s ServiceReadiness) Valid() bool {
	for _, v := range AllServiceReadiness {
		if v == s {
			return true
		}
	}
	return false
}

// ServiceOperationKind is a kebab-case enum on the wire.
type ServiceOperationKind string

const (
	OpStart   ServiceOperationKind = "start"
	OpStop    ServiceOperationKind = "stop"
	OpRestart ServiceOperationKind = "restart"
	OpStatus  ServiceOperationKind = "status"
)

var AllServiceOperationKinds = []ServiceOperationKind{OpStart, OpStop, OpRestart, OpStatus}

func (k ServiceOperationKind) Valid() bool {
	for _, v := range AllServiceOperationKinds {
		if v == k {
			return true
		}
	}
	return false
}

// OperationStatus is a lowercase enum on the wire.
type OperationStatus string

const (
	OpStatusQueued    OperationStatus = "queued"
	OpStatusRunning   OperationStatus = "running"
	OpStatusSucceeded OperationStatus = "succeeded"
	OpStatusFailed    OperationStatus = "failed"
)

var AllOperationStatuses = []OperationStatus{
	OpStatusQueued, OpStatusRunning, OpStatusSucceeded, OpStatusFailed,
}

func (s OperationStatus) Valid() bool {
	for _, v := range AllOperationStatuses {
		if v == s {
			return true
		}
	}
	return false
}

// ReadinessKind is a lowercase enum on the wire.
type ReadinessKind string

const (
	ReadinessKindProcess   ReadinessKind = "process"
	ReadinessKindTCP       ReadinessKind = "tcp"
	ReadinessKindHTTP      ReadinessKind = "http"
	ReadinessKindContainer ReadinessKind = "container"
	ReadinessKindTailnet   ReadinessKind = "tailnet"
	ReadinessKindCommand   ReadinessKind = "command"
	ReadinessKindExit      ReadinessKind = "exit"
	ReadinessKindLog       ReadinessKind = "log"
	ReadinessKindCustom    ReadinessKind = "custom"
)

var AllReadinessKinds = []ReadinessKind{
	ReadinessKindProcess, ReadinessKindTCP, ReadinessKindHTTP, ReadinessKindContainer,
	ReadinessKindTailnet, ReadinessKindCommand, ReadinessKindExit, ReadinessKindLog,
	ReadinessKindCustom,
}

func (k ReadinessKind) Valid() bool {
	for _, v := range AllReadinessKinds {
		if v == k {
			return true
		}
	}
	return false
}

// ProcessIdentity is the serde-untagged union of PosixProcessIdentity and
// DockerContainerIdentity. `Pid` is nil (absent) for docker identities.
type ProcessIdentity struct {
	ManagerInstanceID  string `json:"managerInstanceId"`
	ServiceID          string `json:"serviceId"`
	Generation         uint64 `json:"generation"`
	StartedAt          string `json:"startedAt"`
	CommandFingerprint string `json:"commandFingerprint"`
	// Posix-only fields.
	Pid           *int64  `json:"pid,omitempty"`
	Pgid          *int64  `json:"pgid,omitempty"`
	StartIdentity *string `json:"startIdentity,omitempty"`
	// Docker-only fields (all present iff this is a container identity).
	ContainerName      *string `json:"containerName,omitempty"`
	ContainerID        *string `json:"containerId,omitempty"`
	ContainerStartedAt *string `json:"containerStartedAt,omitempty"`
}

func (p *ProcessIdentity) IsDocker() bool { return p.ContainerName != nil }

func (p *ProcessIdentity) PidValue() int64 {
	if p.Pid == nil {
		return 0
	}
	return *p.Pid
}

func (p *ProcessIdentity) PgidValue() int64 {
	if p.Pgid == nil {
		return 0
	}
	return *p.Pgid
}

func (p *ProcessIdentity) StartIdentityValue() string {
	if p.StartIdentity == nil {
		return ""
	}
	return *p.StartIdentity
}

type ServiceLifecycleState struct {
	ServiceID          string              `json:"serviceId"`
	DesiredState       DesiredServiceState `json:"desiredState"`
	ActualState        ActualServiceState  `json:"actualState"`
	Readiness          ServiceReadiness    `json:"readiness"`
	Generation         uint64              `json:"generation"`
	Identity           *ProcessIdentity    `json:"identity,omitempty"`
	ReadinessKind      *ReadinessKind      `json:"readinessKind,omitempty"`
	ReadinessDetail    *string             `json:"readinessDetail,omitempty"`
	CreatedAt          string              `json:"createdAt"`
	UpdatedAt          string              `json:"updatedAt"`
	ExitedAt           *string             `json:"exitedAt,omitempty"`
	ExitCode           *int32              `json:"exitCode,omitempty"`
	Error              *string             `json:"error,omitempty"`
	CurrentOperationID *string             `json:"currentOperationId,omitempty"`
}

type OperationTraceEntry struct {
	At      string `json:"at"`
	Message string `json:"message"`
}

type OperationError struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

// OperationKind is a kebab-case enum on the wire.
type OperationKind string

const (
	OperationKindService         OperationKind = "service"
	OperationKindBulkStart       OperationKind = "bulk-start"
	OperationKindManagerShutdown OperationKind = "manager-shutdown"
)

type Operation struct {
	ID               string                `json:"id"`
	RequestID        string                `json:"requestId"`
	Kind             OperationKind         `json:"kind"`
	ServiceID        *string               `json:"serviceId,omitempty"`
	TargetServiceIDs *[]string             `json:"targetServiceIds,omitempty"`
	Action           *ServiceOperationKind `json:"action,omitempty"`
	Status           OperationStatus       `json:"status"`
	CreatedAt        string                `json:"createdAt"`
	UpdatedAt        string                `json:"updatedAt"`
	Trace            []OperationTraceEntry `json:"trace"`
	Error            *OperationError       `json:"error,omitempty"`
}

type ManagerEvent struct {
	Sequence uint64                 `json:"sequence"`
	At       string                 `json:"at"`
	Type     string                 `json:"type"`
	Data     map[string]interface{} `json:"data"`
}

type ManagerMetadata struct {
	Version         uint32 `json:"version"`
	ProtocolVersion uint32 `json:"protocolVersion"`
	InstanceID      string `json:"instanceId"`
	Pid             int64  `json:"pid"`
	Port            uint16 `json:"port"`
	StartedAt       string `json:"startedAt"`
}

type ManagerInfo struct {
	ProtocolVersion  uint32 `json:"protocolVersion"`
	InstanceID       string `json:"instanceId"`
	Pid              int64  `json:"pid"`
	Port             uint16 `json:"port"`
	StartedAt        string `json:"startedAt"`
	MetadataVersion  uint32 `json:"metadataVersion"`
	RuntimeDirectory string `json:"runtimeDirectory"`
}

type PersistedManagerState struct {
	Version  uint32                            `json:"version"`
	Services map[string]*ServiceLifecycleState `json:"services"`
}

type LogSlice struct {
	ServiceID  string `json:"serviceId"`
	Generation uint64 `json:"generation"`
	Cursor     uint64 `json:"cursor"`
	NextCursor uint64 `json:"nextCursor"`
	Data       string `json:"data"`
	Reset      bool   `json:"reset"`
	Truncated  bool   `json:"truncated"`
}

type LockOwnershipProof struct {
	Version     uint32          `json:"version"`
	Metadata    ManagerMetadata `json:"metadata"`
	TokenDigest string          `json:"tokenDigest"`
	Signature   string          `json:"signature"`
}

type StaleLockMarker struct {
	Version  uint32             `json:"version"`
	Action   string             `json:"action"` // always "stale-lock"
	Original ManagerMetadata    `json:"original"`
	Proof    LockOwnershipProof `json:"proof"`
}
