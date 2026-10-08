<?php

namespace Tests\Unit;

use App\Support\HearthLink;
use Tests\TestCase;

class HearthLinkTest extends TestCase
{
    private string $root = '';

    protected function setUp(): void
    {
        parent::setUp();
        $this->root = sys_get_temp_dir().'/hearth-link-'.bin2hex(random_bytes(4));
        mkdir($this->root.'/bin', 0777, true);
        mkdir($this->root.'/share', 0777, true);
        config([
            'hearth.bin_link' => $this->root.'/bin/hearth',
            'hearth.versioned_dir' => $this->root.'/share',
        ]);
    }

    protected function tearDown(): void
    {
        $this->removeTree($this->root);
        parent::tearDown();
    }

    public function test_a_missing_link_points_at_the_bundled_binary(): void
    {
        $bundled = $this->bundled('Hearth.app/Contents/MacOS/hearth');

        $this->assertSame('pointed hearth at this app', HearthLink::ensure($bundled, '0.22.0'));
        $this->assertTrue(is_link($this->link()));
        $this->assertSame(realpath($bundled), realpath($this->link()));
    }

    public function test_a_link_inside_an_app_is_retargeted_and_the_old_file_stays(): void
    {
        $old = $this->bundled('Old.app/Contents/extras/hearth', 'old-bytes');
        symlink($old, $this->link());
        $next = $this->bundled('Hearth.app/Contents/MacOS/hearth', 'new-bytes');

        $this->assertSame('pointed hearth at this app', HearthLink::ensure($next, '0.22.0'));
        $this->assertSame('old-bytes', file_get_contents($old));
        $this->assertSame(realpath($next), realpath($this->link()));
    }

    public function test_an_equal_or_newer_install_is_left_alone(): void
    {
        $installed = $this->root.'/share/hearth-0.22.0';
        file_put_contents($installed, 'installed');
        symlink($installed, $this->link());
        $bundled = $this->bundled('Hearth.app/Contents/MacOS/hearth');

        $this->assertSame('left the newer hearth install in place', HearthLink::ensure($bundled, '0.22.0'));
        $this->assertSame(realpath($installed), realpath($this->link()));
        $this->assertSame('installed', file_get_contents($installed));
    }

    public function test_an_older_install_is_retargeted_and_the_old_file_stays(): void
    {
        $installed = $this->root.'/share/hearth-0.21.0';
        file_put_contents($installed, 'old-install');
        symlink($installed, $this->link());
        $bundled = $this->bundled('Hearth.app/Contents/MacOS/hearth');

        $this->assertSame('pointed hearth at this app', HearthLink::ensure($bundled, '0.22.0'));
        $this->assertSame('old-install', file_get_contents($installed));
        $this->assertSame(realpath($bundled), realpath($this->link()));
    }

    public function test_a_regular_file_and_an_unknown_symlink_are_left_alone(): void
    {
        $bundled = $this->bundled('Hearth.app/Contents/MacOS/hearth');
        file_put_contents($this->link(), 'regular');

        $this->assertSame('left the hearth link alone', HearthLink::ensure($bundled, '0.22.0'));
        $this->assertSame('regular', file_get_contents($this->link()));
        $this->assertFalse(is_link($this->link()));

        unlink($this->link());
        $other = $this->root.'/other-hearth';
        file_put_contents($other, 'other');
        symlink($other, $this->link());

        $this->assertSame('left the hearth link alone', HearthLink::ensure($bundled, '0.22.0'));
        $this->assertSame(realpath($other), realpath($this->link()));
    }

    private function link(): string
    {
        return $this->root.'/bin/hearth';
    }

    private function bundled(string $relative, string $bytes = 'bundle'): string
    {
        $path = $this->root.'/'.$relative;
        if (! is_dir(dirname($path))) {
            mkdir(dirname($path), 0777, true);
        }
        file_put_contents($path, $bytes);
        chmod($path, 0755);

        return $path;
    }

    private function removeTree(string $root): void
    {
        if ($root === '' || ! is_dir($root)) {
            return;
        }
        $items = new \RecursiveIteratorIterator(
            new \RecursiveDirectoryIterator($root, \FilesystemIterator::SKIP_DOTS),
            \RecursiveIteratorIterator::CHILD_FIRST,
        );
        foreach ($items as $item) {
            if ($item->isLink() || $item->isFile()) {
                unlink($item->getPathname());
            } else {
                rmdir($item->getPathname());
            }
        }
        rmdir($root);
    }
}
