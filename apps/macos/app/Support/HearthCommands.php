<?php

namespace App\Support;

interface HearthCommands
{
    /**
     * Run the bundled hearth with an argv array. No shell string.
     * A null timeout waits without a bound (shared install and start).
     *
     * @param  list<string>  $args
     */
    public function run(string $cwd, array $args, ?int $timeoutSeconds = 20): CommandResult;
}
