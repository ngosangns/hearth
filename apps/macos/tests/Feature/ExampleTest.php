<?php

namespace Tests\Feature;

// use Illuminate\Foundation\Testing\RefreshDatabase;
use Tests\TestCase;

class ExampleTest extends TestCase
{
    /**
     * A basic test example.
     */
    public function test_the_application_returns_a_successful_response(): void
    {
        $response = $this->get('/');

        $response->assertOk();
        $response->assertSee('Workspaces');
        $response->assertSee('inside asar');
        $response->assertSee('No workspaces yet');
    }

    public function test_the_spike_page_still_checks_the_binary(): void
    {
        $response = $this->get('/spike');

        $response->assertOk();
        $response->assertSee('Hearth macOS spike');
        $response->assertSee('inside asar');
    }

    public function test_ensure_stops_the_fixture_daemon_when_the_binary_is_present(): void
    {
        $response = $this->post(route('spike.ensure'));

        $response->assertOk();
        if (is_executable(base_path('extras/hearth'))) {
            $response->assertSee('200 ok');
        } else {
            $response->assertSee('missing or not executable');
        }
    }
}
