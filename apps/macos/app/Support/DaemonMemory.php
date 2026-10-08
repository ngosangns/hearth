<?php

namespace App\Support;

use Illuminate\Support\Facades\Crypt;
use RuntimeException;
use Throwable;

/**
 * Token and the stopped set for this PHP process.
 *
 * `php -S` drops class statics at the end of every request, so the values live
 * in a SysV segment named by this process id. Livewire never receives the
 * token, and the token is not written to a file. The sidecar file stores only
 * the pid and the segment key so a later process can delete a dead segment.
 *
 * The bundled php-bin has no sysvshm, so without it the payload is stored in a
 * pid-keyed 0600 file encrypted with APP_KEY instead. HEARTH_DAEMON_MEMORY=file
 * forces the file backend even when sysvshm is loaded.
 */
final class DaemonMemory
{
    private const VAR_KEY = 1;

    public static function reset(): void
    {
        self::reapDead();
        self::removeKey(self::key());
        @unlink(self::metaPath());
    }

    /**
     * @param  array<string, mixed>  $payload
     */
    public static function put(string $root, array $payload): void
    {
        $token = $payload['token'] ?? null;
        $port = $payload['port'] ?? null;
        if (! is_string($token) || $token === '' || ! is_numeric($port)) {
            return;
        }
        $port = (int) $port;
        if ($port < 1 || $port > 65535) {
            return;
        }

        $data = self::read();
        $data['sessions'][$root] = [
            'token' => $token,
            'port' => $port,
            'protocolVersion' => $payload['protocolVersion'] ?? null,
            'runtimeDirectory' => isset($payload['runtimeDirectory']) ? (string) $payload['runtimeDirectory'] : null,
        ];
        self::write($data);
    }

    public static function forgetRoot(string $root): void
    {
        $data = self::read();
        unset($data['sessions'][$root]);
        self::write($data);
    }

    public static function forgetId(string $id): void
    {
        $data = self::read();
        unset($data['stopped'][$id]);
        self::write($data);
    }

    public static function hasToken(string $root): bool
    {
        return isset(self::read()['sessions'][$root]);
    }

    public static function token(string $root): ?string
    {
        $token = self::read()['sessions'][$root]['token'] ?? null;

        return is_string($token) && $token !== '' ? $token : null;
    }

    /**
     * @return ?array{port: int, protocolVersion: mixed, runtimeDirectory: ?string, hasToken: true}
     */
    public static function publicSession(string $root): ?array
    {
        $row = self::read()['sessions'][$root] ?? null;
        if (! is_array($row)) {
            return null;
        }

        return [
            'port' => (int) $row['port'],
            'protocolVersion' => $row['protocolVersion'] ?? null,
            'runtimeDirectory' => isset($row['runtimeDirectory']) ? (string) $row['runtimeDirectory'] : null,
            'hasToken' => true,
        ];
    }

    public static function markStopped(string $id): void
    {
        $data = self::read();
        $data['stopped'][$id] = true;
        self::write($data);
    }

    public static function clearStopped(string $id): void
    {
        $data = self::read();
        unset($data['stopped'][$id]);
        self::write($data);
    }

    public static function isStopped(string $id): bool
    {
        return isset(self::read()['stopped'][$id]);
    }

    /**
     * @return array{sessions: array<string, array{token: string, port: int, protocolVersion: mixed, runtimeDirectory: ?string}>, stopped: array<string, true>}
     */
    private static function read(): array
    {
        self::reapDead();
        $raw = self::shm() ? @shm_get_var(self::attach(), self::VAR_KEY) : self::readFile();
        $sessions = [];
        $stopped = [];
        if (is_array($raw)) {
            if (isset($raw['sessions']) && is_array($raw['sessions'])) {
                $sessions = $raw['sessions'];
            }
            if (isset($raw['stopped']) && is_array($raw['stopped'])) {
                $stopped = $raw['stopped'];
            }
        }

        return ['sessions' => $sessions, 'stopped' => $stopped];
    }

    /**
     * @param  array{sessions: array<string, mixed>, stopped: array<string, mixed>}  $data
     */
    private static function write(array $data): void
    {
        if (! self::shm()) {
            self::writeFile($data);

            return;
        }
        if (! shm_put_var(self::attach(), self::VAR_KEY, $data)) {
            throw new RuntimeException('daemon memory could not be stored');
        }
    }

    private static function shm(): bool
    {
        return function_exists('shm_attach') && env('HEARTH_DAEMON_MEMORY', 'auto') !== 'file';
    }

    private static function readFile(): mixed
    {
        $raw = @file_get_contents(self::metaPath());
        if (! is_string($raw) || $raw === '') {
            return null;
        }
        try {
            return json_decode(Crypt::decryptString($raw), true);
        } catch (Throwable) {
            return null;
        }
    }

    private static function writeFile(array $data): void
    {
        $path = self::metaPath();
        $dir = dirname($path);
        if (! is_dir($dir)) {
            mkdir($dir, 0700, true);
        }
        file_put_contents($path, Crypt::encryptString(json_encode($data)), LOCK_EX);
        @chmod($path, 0600);
    }

    private static function attach(): \SysvSharedMemory
    {
        $key = self::key();
        $segment = @shm_attach($key, 65536, 0600);
        if ($segment === false) {
            throw new RuntimeException('daemon memory is unavailable');
        }
        self::remember($key);

        return $segment;
    }

    private static function key(): int
    {
        $key = crc32(base_path().'|hearth-daemon-memory|'.getmypid()) & 0x7FFFFFFF;

        return $key === 0 ? 1 : $key;
    }

    private static function remember(int $key): void
    {
        $path = self::metaPath();
        $dir = dirname($path);
        if (! is_dir($dir)) {
            mkdir($dir, 0700, true);
        }
        $body = getmypid()."\n".$key."\n";
        if (@file_get_contents($path) !== $body) {
            file_put_contents($path, $body, LOCK_EX);
        }
    }

    private static function metaPath(): string
    {
        return storage_path('framework/cache/hearth-daemon-memory-'.getmypid());
    }

    private static function reapDead(): void
    {
        foreach (glob(storage_path('framework/cache/hearth-daemon-memory-*')) ?: [] as $file) {
            $name = basename($file);
            $pid = (int) substr($name, strrpos($name, '-') + 1);
            if ($pid === getmypid() || self::pidAlive($pid)) {
                continue;
            }
            // Legacy shm sidecar bodies are "pid\nkey"; drop the segment too.
            $raw = @file_get_contents($file);
            if (is_string($raw)) {
                $parts = explode("\n", trim($raw));
                if (count($parts) === 2 && ctype_digit($parts[0]) && ctype_digit($parts[1])) {
                    self::removeKey((int) $parts[1]);
                }
            }
            @unlink($file);
        }
    }

    private static function removeKey(int $key): void
    {
        if ($key <= 0 || ! self::shm()) {
            return;
        }
        $segment = @shm_attach($key, 65536, 0600);
        if ($segment === false) {
            return;
        }
        @shm_remove($segment);
        @shm_detach($segment);
    }

    private static function pidAlive(int $pid): bool
    {
        if ($pid <= 0) {
            return true;
        }
        if (function_exists('posix_kill')) {
            if (posix_kill($pid, 0)) {
                return true;
            }

            return posix_strerror(posix_get_last_error()) !== 'No such process';
        }
        // Bundled php-bin has no posix; kill -0 still answers liveness on macOS.
        if (function_exists('exec')) {
            @exec('kill -0 '.$pid.' 2>/dev/null', $out, $code);

            return $code === 0;
        }

        return true;
    }
}
