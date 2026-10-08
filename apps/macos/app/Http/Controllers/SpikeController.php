<?php

namespace App\Http\Controllers;

use App\Support\HearthBinary;
use Illuminate\Http\Client\ConnectionException;
use Illuminate\Support\Facades\Http;
use Illuminate\Support\Facades\Process;
use Illuminate\View\View;

class SpikeController extends Controller
{
    public function show(HearthBinary $hearth): View
    {
        return view('spike', [
            'report' => $hearth->report(),
            'version' => $hearth->version(),
            'ensure' => null,
        ]);
    }

    public function ensure(HearthBinary $hearth): View
    {
        $report = $hearth->report();
        $version = $hearth->version();
        $binary = $hearth->path();

        if (! $report['executable']) {
            return view('spike', [
                'report' => $report,
                'version' => $version,
                'ensure' => [
                    'ok' => false,
                    'detail' => 'bundled hearth is missing or not executable',
                ],
            ]);
        }

        $root = storage_path('app/spike-fixture');
        if (! is_dir($root) && ! mkdir($root, 0755, true) && ! is_dir($root)) {
            return view('spike', [
                'report' => $report,
                'version' => $version,
                'ensure' => [
                    'ok' => false,
                    'detail' => "cannot create {$root}",
                ],
            ]);
        }

        file_put_contents($root.'/hearth.yaml', <<<'YAML'
version: 1
services:
  spike:
    run: { argv: ["/bin/echo", "hearth-macos-spike"] }
    readiness: { kind: exit }
YAML);

        $ensure = Process::timeout(90)->run([
            $binary,
            '--root',
            $root,
            'manager',
            'ensure',
            '--json',
        ]);

        $stdout = $ensure->output();
        $payload = $this->jsonPayload($stdout);
        $health = null;
        if (is_array($payload) && isset($payload['port'])) {
            $health = $this->healthz((int) $payload['port']);
        }

        $stop = Process::timeout(90)->run([
            $binary,
            '--root',
            $root,
            'manager',
            'stop',
        ]);

        return view('spike', [
            'report' => $report,
            'version' => $version,
            'ensure' => [
                'ok' => $ensure->successful() && $health !== null && $health['ok'] && $stop->successful(),
                'exit' => $ensure->exitCode(),
                'output' => $this->redact(trim($stdout)),
                'error' => trim($ensure->errorOutput()),
                'health' => $health,
                'stop_exit' => $stop->exitCode(),
                'stop_output' => $this->redact(trim($stop->output()."\n".$stop->errorOutput())),
                'root' => $root,
            ],
        ]);
    }

    /**
     * @return ?array<string, mixed>
     */
    private function jsonPayload(string $stdout): ?array
    {
        $decoded = null;
        foreach (preg_split("/\r\n|\n|\r/", $stdout) ?: [] as $line) {
            $line = trim($line);
            if ($line === '' || ! str_starts_with($line, '{')) {
                continue;
            }
            $candidate = json_decode($line, true);
            if (is_array($candidate)) {
                $decoded = $candidate;
            }
        }

        return $decoded;
    }

    /**
     * @return array{ok: bool, status: ?int, body: string}
     */
    private function healthz(int $port): array
    {
        try {
            $response = Http::timeout(5)->get("http://127.0.0.1:{$port}/healthz");
        } catch (ConnectionException $exception) {
            return [
                'ok' => false,
                'status' => null,
                'body' => $exception->getMessage(),
            ];
        }

        return [
            'ok' => $response->successful(),
            'status' => $response->status(),
            'body' => $response->body(),
        ];
    }

    private function redact(string $text): string
    {
        return preg_replace('/("token"\s*:\s*")[^"]+/', '$1[redacted]', $text) ?? $text;
    }
}
