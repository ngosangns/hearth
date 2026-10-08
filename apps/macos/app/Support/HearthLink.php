<?php

namespace App\Support;

/**
 * On first open, point ~/.local/bin/hearth at the binary inside the app when
 * that link is missing, already points inside an .app, or points at an older
 * task-install build. A newer or equal install, a regular file, and an unknown
 * symlink stay as they are. The versioned file is never deleted.
 */
final class HearthLink
{
    public static function ensure(string $bundledPath, string $appVersion): string
    {
        $link = (string) config('hearth.bin_link');
        $versioned = rtrim((string) config('hearth.versioned_dir'), '/');
        if ($link === '' || ! is_file($bundledPath)) {
            return 'left the hearth link alone';
        }
        $bundled = realpath($bundledPath);
        if ($bundled === false) {
            return 'left the hearth link alone';
        }

        if (! is_link($link) && ! file_exists($link)) {
            return self::point($link, $bundled);
        }
        if (! is_link($link)) {
            return 'left the hearth link alone';
        }

        $target = readlink($link);
        if ($target === false) {
            return 'left the hearth link alone';
        }
        $absolute = self::absolute(dirname($link), $target);
        $current = realpath($link);
        if ($absolute === $bundled || $current === $bundled) {
            return 'hearth already points at this app';
        }
        if (self::insideApp($absolute)) {
            return self::point($link, $bundled);
        }
        $installed = self::versionedVersion($absolute, $versioned);
        if ($installed !== null) {
            if (version_compare($installed, $appVersion, '>=')) {
                return 'left the newer hearth install in place';
            }

            return self::point($link, $bundled);
        }

        return 'left the hearth link alone';
    }

    public static function insideApp(string $path): bool
    {
        return str_contains($path, '.app/Contents/');
    }

    private static function point(string $link, string $bundled): string
    {
        if (is_file($link) && ! is_link($link)) {
            return 'left the hearth link alone';
        }
        $dir = dirname($link);
        if (! is_dir($dir) && ! mkdir($dir, 0755, true) && ! is_dir($dir)) {
            return 'could not create the hearth link directory';
        }
        if (is_link($link) && ! unlink($link)) {
            return 'could not retarget the hearth link';
        }
        if (! symlink($bundled, $link)) {
            return 'could not point hearth at this app';
        }

        return 'pointed hearth at this app';
    }

    private static function absolute(string $dir, string $target): string
    {
        if (str_starts_with($target, '/')) {
            return $target;
        }

        return $dir.'/'.$target;
    }

    private static function versionedVersion(string $absolute, string $versioned): ?string
    {
        if ($versioned === '') {
            return null;
        }
        $file = realpath($absolute) ?: $absolute;
        $dir = realpath($versioned) ?: $versioned;
        if (dirname($file) !== rtrim($dir, '/')) {
            return null;
        }
        if (preg_match('/^hearth-(\d+\.\d+\.\d+)$/', basename($file), $match) === 1) {
            return $match[1];
        }

        return null;
    }
}
