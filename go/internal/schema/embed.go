package schema

import "embed"

//go:embed v1/*.json
var schemaFS embed.FS
