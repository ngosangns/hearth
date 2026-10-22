package cli

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/ngosangns/hearth/go/internal/omap"
)

type workspaceRow struct {
	ID      string `json:"id"`
	Path    string `json:"path"`
	Trusted bool   `json:"trusted"`
	AddedAt string `json:"addedAt"`
}

func workspacesCommand(rest []string, io *Io) (int, error) {
	flags, err := ParseCommandFlags(rest, []Flag{FlagJSON})
	if err != nil {
		return finishNew(io, hasFlag(rest, "--json"), err)
	}
	if len(flags.Positionals) != 1 || flags.Positionals[0] != "list" {
		return finishNew(io, flags.JSON, usageErr(workspacesUsage))
	}
	rows, err := readWorkspaces()
	if err != nil {
		return finishNew(io, flags.JSON, err)
	}
	if flags.JSON {
		items := make([]any, 0, len(rows))
		for _, row := range rows {
			item := omap.New()
			item.Set("id", row.ID)
			item.Set("path", row.Path)
			item.Set("trusted", row.Trusted)
			item.Set("addedAt", row.AddedAt)
			items = append(items, item)
		}
		doc := omap.New()
		doc.Set("workspaces", items)
		out(io, compactJSON(doc))
		return 0, nil
	}
	for _, row := range rows {
		trusted := "untrusted"
		if row.Trusted {
			trusted = "trusted"
		}
		out(io, fmt.Sprintf("%s  %s  %s  %s", row.ID, row.Path, trusted, row.AddedAt))
	}
	return 0, nil
}

func workspacesPath() (string, error) {
	if path := os.Getenv("HEARTH_WORKSPACE_FILE"); path != "" {
		return path, nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", failErr(ExitFailed, "cannot resolve the home directory: "+err.Error())
	}
	return filepath.Join(home, "Library", "Application Support", "HearthApp", "workspaces.json"), nil
}

func readWorkspaces() ([]workspaceRow, error) {
	path, err := workspacesPath()
	if err != nil {
		return nil, err
	}
	body, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			return []workspaceRow{}, nil
		}
		return nil, failErr(ExitFailed, "cannot read workspaces: "+err.Error())
	}
	if strings.TrimSpace(string(body)) == "" {
		return []workspaceRow{}, nil
	}
	var raw []json.RawMessage
	if err := json.Unmarshal(body, &raw); err != nil {
		return nil, failErr(ExitProtocol, "workspaces file is not a JSON array")
	}
	rows := make([]workspaceRow, 0, len(raw))
	for _, item := range raw {
		var row workspaceRow
		if err := json.Unmarshal(item, &row); err != nil {
			return nil, failErr(ExitProtocol, "workspaces file has an invalid row")
		}
		if row.ID == "" || row.Path == "" {
			return nil, failErr(ExitProtocol, "workspaces file row needs id and path")
		}
		rows = append(rows, row)
	}
	return rows, nil
}
