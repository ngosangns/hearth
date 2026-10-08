<?php

namespace Tests\Unit;

use App\Support\WorkspaceStore;
use InvalidArgumentException;
use Tests\TestCase;

class WorkspaceStoreTest extends TestCase
{
    public function test_add_writes_an_untrusted_row_the_tui_can_read(): void
    {
        $dir = $this->makeDir();
        $added = WorkspaceStore::instance()->add($dir);

        $this->assertTrue($added['created']);
        $this->assertFalse($added['record']['trusted']);
        $this->assertMatchesRegularExpression(
            '/^[0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12}$/',
            $added['record']['id'],
        );
        $this->assertMatchesRegularExpression(
            '/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/',
            $added['record']['addedAt'],
        );
        $this->assertStringNotContainsString('.', $added['record']['addedAt']);

        $decoded = json_decode((string) file_get_contents($this->workspaceFile), true);
        $this->assertSame(['id', 'path', 'trusted', 'addedAt'], array_keys($decoded[0]));
        $this->assertFalse($decoded[0]['trusted']);
        $this->assertSame($dir, $decoded[0]['path']);
    }

    public function test_adding_the_same_folder_does_not_change_trust(): void
    {
        $dir = $this->makeDir();
        $store = WorkspaceStore::instance();
        $added = $store->add($dir);
        $store->trust($added['record']['id']);

        $again = $store->add($dir);

        $this->assertFalse($again['created']);
        $this->assertTrue($again['record']['trusted']);
        $this->assertCount(1, $store->rows());
    }

    public function test_relative_missing_and_file_paths_are_refused(): void
    {
        $store = WorkspaceStore::instance();

        $this->expectException(InvalidArgumentException::class);
        $store->add('relative/folder');
    }

    public function test_a_missing_folder_is_refused(): void
    {
        $this->expectException(InvalidArgumentException::class);
        $this->expectExceptionMessage('folder does not exist');
        WorkspaceStore::instance()->add('/tmp/hearth-macos-missing-'.bin2hex(random_bytes(4)));
    }

    public function test_a_file_is_not_a_folder(): void
    {
        $file = $this->workspaceFile.'.not-a-dir';
        file_put_contents($file, 'x');

        try {
            WorkspaceStore::instance()->add($file);
            $this->fail('a file must not be added');
        } catch (InvalidArgumentException $error) {
            $this->assertStringContainsString('folder does not exist', $error->getMessage());
        } finally {
            @unlink($file);
        }
    }

    public function test_tilde_expands_and_a_missing_child_is_refused(): void
    {
        try {
            WorkspaceStore::instance()->add('~/hearth-macos-no-such-'.bin2hex(random_bytes(4)));
            $this->fail('missing home child must be refused');
        } catch (InvalidArgumentException $error) {
            $this->assertStringContainsString((string) getenv('HOME'), $error->getMessage());
        }
    }

    public function test_corrupt_file_is_quarantined_on_open_only(): void
    {
        $dir = $this->makeDir();
        $store = WorkspaceStore::instance();
        $store->add($dir);
        file_put_contents($this->workspaceFile, '{not-json');

        $error = $store->reload();

        $this->assertNotNull($error);
        $this->assertStringContainsString('could not be read', $error);
        $this->assertCount(1, $store->rows());
        $this->assertFileExists($this->workspaceFile);

        WorkspaceStore::reset();
        $reopened = WorkspaceStore::instance();

        $this->assertSame([], $reopened->rows());
        $this->assertNotNull($reopened->loadError);
        $this->assertStringContainsString('moved to', (string) $reopened->loadError);
        $this->assertFileDoesNotExist($this->workspaceFile);
        $this->assertNotEmpty(glob($this->workspaceFile.'.corrupt-*') ?: []);
    }

    public function test_lowercase_ids_already_on_disk_are_kept(): void
    {
        $dir = $this->makeDir();
        file_put_contents($this->workspaceFile, json_encode([[
            'id' => 'abc',
            'path' => $dir,
            'trusted' => true,
            'addedAt' => '2020-01-02T03:04:05Z',
        ]]));

        $row = WorkspaceStore::instance()->rows()[0];

        $this->assertSame('abc', $row['id']);
        $this->assertTrue($row['trusted']);
        $this->assertSame('2020-01-02T03:04:05Z', $row['addedAt']);
    }

    public function test_forget_removes_the_row(): void
    {
        $dir = $this->makeDir();
        $store = WorkspaceStore::instance();
        $added = $store->add($dir);

        $this->assertTrue($store->remove($added['record']['id']));
        $this->assertSame([], json_decode((string) file_get_contents($this->workspaceFile), true));
    }

    private function makeDir(): string
    {
        $dir = sys_get_temp_dir().'/hearth-macos-dir-'.bin2hex(random_bytes(4));
        mkdir($dir);
        $this->beforeApplicationDestroyed(function () use ($dir) {
            @rmdir($dir);
        });

        return realpath($dir) ?: $dir;
    }
}
