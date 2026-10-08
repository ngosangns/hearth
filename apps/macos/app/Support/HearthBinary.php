<?php

namespace App\Support;

use Illuminate\Support\Facades\Process;
use Illuminate\Support\Facades\Storage;

class HearthBinary
{
    public function path(): string
    {
        if (config('filesystems.disks.extras')) {
            return Storage::disk('extras')->path('hearth');
        }

        return base_path('extras/hearth');
    }

    /**
     * @return array{path: string, real: ?string, exists: bool, executable: bool, inside_asar: bool}
     */
    public function report(): array
    {
        $path = $this->path();
        $real = realpath($path);

        return [
            'path' => $path,
            'real' => $real === false ? null : $real,
            'exists' => is_file($path),
            'executable' => is_file($path) && is_executable($path),
            'inside_asar' => $this->insideAsar($real === false ? $path : $real),
        ];
    }

    public function insideAsar(string $path): bool
    {
        return str_contains($path, '.asar') && ! str_contains($path, '.asar.unpacked');
    }

    /**
     * @return array{ok: bool, exit: ?int, output: string}
     */
    public function version(): array
    {
        $path = $this->path();
        if (! is_file($path) || ! is_executable($path)) {
            return [
                'ok' => false,
                'exit' => null,
                'output' => 'bundled hearth is missing or not executable',
            ];
        }

        $result = Process::timeout(20)->run([$path, '--version']);

        return [
            'ok' => $result->successful(),
            'exit' => $result->exitCode(),
            'output' => trim($result->output().$result->errorOutput()),
        ];
    }
}
