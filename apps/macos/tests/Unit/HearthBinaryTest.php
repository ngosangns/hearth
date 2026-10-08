<?php

namespace Tests\Unit;

use App\Support\HearthBinary;
use PHPUnit\Framework\TestCase;

class HearthBinaryTest extends TestCase
{
    public function test_a_path_inside_app_asar_is_not_executable(): void
    {
        $binary = new HearthBinary;

        $this->assertTrue($binary->insideAsar(
            '/Applications/Hearth.app/Contents/Resources/app.asar/extras/hearth'
        ));
    }

    public function test_an_unpacked_or_contents_path_is_outside_asar(): void
    {
        $binary = new HearthBinary;

        $this->assertFalse($binary->insideAsar(
            '/Applications/Hearth.app/Contents/extras/hearth'
        ));
        $this->assertFalse($binary->insideAsar(
            '/Applications/Hearth.app/Contents/Resources/app.asar.unpacked/extras/hearth'
        ));
    }
}
