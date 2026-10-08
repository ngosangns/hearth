<?php

namespace App\Support;

use Illuminate\Support\Facades\Process;
use Throwable;

final class BundledHearth implements HearthCommands, RunsHearth
{
    public function __construct(private HearthBinary $binary) {}

    public function manager(string $root, string $subcommand): CommandResult
    {
        $timeout = match ($subcommand) {
            'ensure' => 90,
            'stop', 'restart' => 300,
            'reload' => 60,
            default => 20,
        };

        return $this->run($root, ['--root', $root, 'manager', $subcommand, '--json'], $timeout);
    }

    public function run(string $cwd, array $args, ?int $timeoutSeconds = 20): CommandResult
    {
        $path = $this->binary->path();
        if (! is_file($path) || ! is_executable($path)) {
            return CommandResult::missingBinary();
        }

        try {
            $pending = Process::path($cwd);
            // php -S is the cli-server SAPI. Symfony then keeps only getenv() keys
            // that also exist on $_SERVER, which drops HOME. hearth resolves
            // ~/.hearth/shared from HOME, so the child must see the real environment.
            $inherited = getenv();
            if (is_array($inherited) && $inherited !== []) {
                $pending = $pending->env($inherited);
            }
            $pending = $timeoutSeconds === null ? $pending->forever() : $pending->timeout($timeoutSeconds);
            $result = $pending->run([$path, ...$args]);
        } catch (Throwable $error) {
            return new CommandResult(false, null, '', $error->getMessage(), null);
        }

        return CommandResult::fromStreams(
            $result->successful(),
            $result->exitCode(),
            $result->output(),
            $result->errorOutput(),
        );
    }
}
