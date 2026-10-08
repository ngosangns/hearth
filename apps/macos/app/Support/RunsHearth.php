<?php

namespace App\Support;

interface RunsHearth
{
    /**
     * Run `hearth --root <root> manager <subcommand> --json`.
     * `status` discovers a live daemon. `ensure` spawns one. `stop` shuts it down.
     */
    public function manager(string $root, string $subcommand): CommandResult;
}
