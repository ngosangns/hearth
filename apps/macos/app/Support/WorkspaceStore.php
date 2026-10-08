<?php

namespace App\Support;

use Illuminate\Support\Str;
use InvalidArgumentException;
use JsonException;
use RuntimeException;

/**
 * `~/Library/Application Support/HearthApp/workspaces.json`, shared with `hearth tui`.
 * Rows are `{ id, path, trusted, addedAt }` in document order. `addedAt` is ISO-8601 UTC
 * with no fractional seconds. A file that does not decode is moved aside on the first
 * open of this process. A later reload keeps the in-memory list and leaves the file put.
 */
final class WorkspaceStore
{
    private static ?self $instance = null;

    /** @var list<array{id: string, path: string, trusted: bool, addedAt: string}> */
    private array $rows = [];

    public ?string $loadError = null;

    public function __construct(private string $path)
    {
        $dir = dirname($path);
        if ($dir !== '' && $dir !== '.' && ! is_dir($dir)) {
            if (! mkdir($dir, 0755, true) && ! is_dir($dir)) {
                throw new RuntimeException("cannot create {$dir}");
            }
        }
        $this->loadInitial();
    }

    public static function instance(): self
    {
        $path = config('hearth.workspace_file');
        if (! is_string($path) || $path === '') {
            throw new RuntimeException('hearth.workspace_file is not set');
        }
        if (app()->runningUnitTests() && str_contains($path, 'HearthApp')) {
            throw new RuntimeException('tests must not open the real HearthApp workspace file');
        }
        if (self::$instance === null || self::$instance->path !== $path) {
            self::$instance = new self($path);
        }

        return self::$instance;
    }

    public static function reset(): void
    {
        self::$instance = null;
    }

    /**
     * @return list<array{id: string, path: string, trusted: bool, addedAt: string}>
     */
    public function rows(): array
    {
        return $this->rows;
    }

    /**
     * @return ?array{id: string, path: string, trusted: bool, addedAt: string}
     */
    public function get(string $id): ?array
    {
        foreach ($this->rows as $row) {
            if ($row['id'] === $id) {
                return $row;
            }
        }

        return null;
    }

    /**
     * New folders start untrusted. Adding the same folder again returns the existing row.
     *
     * @return array{record: array{id: string, path: string, trusted: bool, addedAt: string}, created: bool}
     */
    public function add(string $input): array
    {
        $path = self::normalize($input);
        if (! is_dir($path)) {
            throw new InvalidArgumentException("folder does not exist: {$path}");
        }
        foreach ($this->rows as $row) {
            if ($row['path'] === $path) {
                return ['record' => $row, 'created' => false];
            }
        }

        $record = [
            'id' => strtoupper((string) Str::uuid()),
            'path' => $path,
            'trusted' => false,
            'addedAt' => self::addedNow(),
        ];
        $this->rows[] = $record;
        try {
            $this->save();
        } catch (RuntimeException $error) {
            array_pop($this->rows);
            throw $error;
        }

        return ['record' => $record, 'created' => true];
    }

    /**
     * @return array{id: string, path: string, trusted: bool, addedAt: string}
     */
    public function trust(string $id): array
    {
        $index = $this->indexOf($id);
        if ($index === null) {
            throw new RuntimeException('workspace not found');
        }
        $previous = $this->rows[$index]['trusted'];
        $this->rows[$index]['trusted'] = true;
        try {
            $this->save();
        } catch (RuntimeException $error) {
            $this->rows[$index]['trusted'] = $previous;
            throw $error;
        }

        return $this->rows[$index];
    }

    public function remove(string $id): bool
    {
        $index = $this->indexOf($id);
        if ($index === null) {
            return false;
        }
        $removed = $this->rows[$index];
        array_splice($this->rows, $index, 1);
        try {
            $this->save();
        } catch (RuntimeException $error) {
            array_splice($this->rows, $index, 0, [$removed]);
            throw $error;
        }

        return true;
    }

    /**
     * Re-read the file. A parse failure keeps the current rows and does not quarantine.
     */
    public function reload(): ?string
    {
        if (! is_file($this->path)) {
            $this->rows = [];
            $this->loadError = null;

            return null;
        }
        $data = file_get_contents($this->path);
        if ($data === false) {
            return "could not read {$this->path}";
        }
        if ($data === '') {
            $this->rows = [];
            $this->loadError = null;

            return null;
        }
        try {
            $this->rows = $this->decode($data);
            $this->loadError = null;

            return null;
        } catch (RuntimeException $error) {
            return "{$this->path} could not be read: {$error->getMessage()}";
        }
    }

    public static function normalize(string $input): string
    {
        if ($input === '' || str_contains($input, "\0")) {
            throw new InvalidArgumentException('path must be an absolute folder');
        }
        if ($input === '~' || str_starts_with($input, '~/')) {
            $home = getenv('HOME') ?: '';
            if ($home === '') {
                throw new InvalidArgumentException('HOME is not set');
            }
            $input = $input === '~' ? $home : $home.substr($input, 1);
        }
        if (! str_starts_with($input, '/')) {
            throw new InvalidArgumentException('path must be absolute');
        }
        if (file_exists($input)) {
            $real = realpath($input);
            if ($real === false) {
                throw new InvalidArgumentException('path must be absolute');
            }
            $input = $real;
        }

        return $input;
    }

    public static function displayPath(string $path): string
    {
        $home = getenv('HOME') ?: '';
        if ($home === '') {
            return $path;
        }
        $home = realpath($home) ?: $home;
        if ($path === $home) {
            return '~';
        }
        $prefix = $home.'/';
        if (str_starts_with($path, $prefix)) {
            return '~/'.substr($path, strlen($prefix));
        }

        return $path;
    }

    public static function folderName(string $path): string
    {
        $name = basename($path);

        return $name === '' || $name === '/' ? $path : $name;
    }

    public static function addedNow(): string
    {
        return gmdate('Y-m-d\TH:i:s\Z');
    }

    private function loadInitial(): void
    {
        if (! is_file($this->path)) {
            return;
        }
        $data = file_get_contents($this->path);
        if ($data === false) {
            $this->loadError = "could not read {$this->path}";

            return;
        }
        if ($data === '') {
            return;
        }
        try {
            $this->rows = $this->decode($data);
        } catch (RuntimeException $error) {
            $this->quarantine($error->getMessage());
        }
    }

    private function quarantine(string $error): void
    {
        $name = basename($this->path);
        $aside = dirname($this->path).'/'.$name.'.corrupt-'.time();
        if (@rename($this->path, $aside)) {
            $this->loadError = "{$name} could not be read and was moved to {$aside}. Starting with an empty workspace list.";

            return;
        }
        $this->loadError = "{$name} could not be read: {$error}";
    }

    /**
     * @return list<array{id: string, path: string, trusted: bool, addedAt: string}>
     */
    private function decode(string $data): array
    {
        try {
            $decoded = json_decode($data, true, 512, JSON_THROW_ON_ERROR);
        } catch (JsonException $error) {
            throw new RuntimeException($error->getMessage());
        }
        if (! is_array($decoded) || ! array_is_list($decoded)) {
            throw new RuntimeException('expected a list');
        }
        $rows = [];
        foreach ($decoded as $row) {
            if (! is_array($row) || array_is_list($row)) {
                throw new RuntimeException('expected an object');
            }
            foreach (['id', 'path', 'addedAt'] as $key) {
                if (! array_key_exists($key, $row) || ! is_string($row[$key])) {
                    throw new RuntimeException("{$key} must be a string");
                }
            }
            if (! array_key_exists('trusted', $row) || ! is_bool($row['trusted'])) {
                throw new RuntimeException('trusted must be a boolean');
            }
            $rows[] = [
                'id' => $row['id'],
                'path' => $row['path'],
                'trusted' => $row['trusted'],
                'addedAt' => $row['addedAt'],
            ];
        }

        return $rows;
    }

    private function save(): void
    {
        $json = json_encode(
            array_values($this->rows),
            JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE,
        );
        if ($json === false) {
            throw new RuntimeException('cannot encode workspaces');
        }
        $tmp = $this->path.'.tmp';
        if (file_put_contents($tmp, $json."\n") === false) {
            throw new RuntimeException("cannot write {$tmp}");
        }
        if (! rename($tmp, $this->path)) {
            @unlink($tmp);
            throw new RuntimeException("cannot save {$this->path}");
        }
    }

    private function indexOf(string $id): ?int
    {
        foreach ($this->rows as $index => $row) {
            if ($row['id'] === $id) {
                return $index;
            }
        }

        return null;
    }
}
