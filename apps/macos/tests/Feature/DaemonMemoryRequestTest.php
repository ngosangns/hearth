<?php

namespace Tests\Feature;

use App\Support\DaemonMemory;
use Illuminate\Support\Facades\Process;
use Tests\TestCase;

class DaemonMemoryRequestTest extends TestCase
{
    public function test_token_and_stopped_flag_survive_the_next_http_request(): void
    {
        if (! function_exists('shm_attach')) {
            $this->markTestSkipped('sysvshm is not loaded');
        }

        $this->runMemoryScenario();
    }

    public function test_file_backend_when_sysvshm_is_unavailable(): void
    {
        $this->runMemoryScenario(['HEARTH_DAEMON_MEMORY' => 'file']);
    }

    /**
     * @param  array<string, string>  $env
     */
    private function runMemoryScenario(array $env = []): void
    {
        $token = 'desk-memory-token';
        $root = '/tmp/hearth-macos-memory-'.bin2hex(random_bytes(4));
        $id = 'MEMORY-ROW';
        $script = tempnam(sys_get_temp_dir(), 'hearth-memory-');
        $this->assertNotFalse($script);
        file_put_contents($script, $this->router($token, $root, $id));

        $port = $this->freePort();
        $server = Process::path(base_path())->env($env)->start([PHP_BINARY, '-S', '127.0.0.1:'.$port, $script]);
        try {
            $this->waitForHttp($port);
            $put = Process::timeout(10)->run(['curl', '-fsS', 'http://127.0.0.1:'.$port.'/put']);
            $this->assertTrue($put->successful(), $put->errorOutput());
            $get = Process::timeout(10)->run(['curl', '-fsS', 'http://127.0.0.1:'.$port.'/get']);
            $this->assertTrue($get->successful(), $get->errorOutput());
            $body = json_decode($get->output(), true);
            $this->assertSame($token, $body['token'] ?? null);
            $this->assertTrue($body['stopped'] ?? false);

            $leaked = false;
            foreach (glob(storage_path('framework/cache/hearth-daemon-memory-*')) ?: [] as $file) {
                $raw = (string) file_get_contents($file);
                if (str_contains($raw, $token)) {
                    $leaked = true;
                }
            }
            $this->assertFalse($leaked);
            Process::timeout(10)->run(['curl', '-fsS', 'http://127.0.0.1:'.$port.'/reset']);
        } finally {
            $server->stop(3);
            @unlink($script);
            DaemonMemory::reset();
        }
    }

    private function router(string $token, string $root, string $id): string
    {
        $base = var_export(base_path(), true);
        $token = var_export($token, true);
        $root = var_export($root, true);
        $id = var_export($id, true);

        return <<<PHP
<?php
require {$base}.'/vendor/autoload.php';
\$app = require {$base}.'/bootstrap/app.php';
\$app->make(Illuminate\Contracts\Console\Kernel::class)->bootstrap();
\$path = parse_url(\$_SERVER['REQUEST_URI'] ?? '/', PHP_URL_PATH);
if (\$path === '/put') {
    App\Support\DaemonMemory::put({$root}, ['token' => {$token}, 'port' => 59997, 'protocolVersion' => 3]);
    App\Support\DaemonMemory::markStopped({$id});
    echo 'ok';
    return;
}
if (\$path === '/reset') {
    App\Support\DaemonMemory::reset();
    echo 'ok';
    return;
}
header('Content-Type: application/json');
echo json_encode([
    'token' => App\Support\DaemonMemory::token({$root}),
    'stopped' => App\Support\DaemonMemory::isStopped({$id}),
]);
PHP;
    }

    private function freePort(): int
    {
        $socket = stream_socket_server('tcp://127.0.0.1:0');
        $this->assertNotFalse($socket);
        $name = stream_socket_get_name($socket, false);
        fclose($socket);
        $this->assertIsString($name);

        return (int) substr($name, strrpos($name, ':') + 1);
    }

    private function waitForHttp(int $port): void
    {
        $deadline = microtime(true) + 10;
        while (microtime(true) < $deadline) {
            $probe = Process::timeout(2)->run(['curl', '-fsS', 'http://127.0.0.1:'.$port.'/get']);
            if ($probe->successful()) {
                return;
            }
            usleep(50000);
        }
        $this->fail('memory server did not answer');
    }
}
