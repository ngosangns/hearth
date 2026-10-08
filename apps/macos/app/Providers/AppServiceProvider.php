<?php

namespace App\Providers;

use App\Support\BundledHearth;
use App\Support\HearthCommands;
use App\Support\RunsHearth;
use Illuminate\Support\ServiceProvider;

class AppServiceProvider extends ServiceProvider
{
    /**
     * Register any application services.
     */
    public function register(): void
    {
        $this->app->bind(RunsHearth::class, BundledHearth::class);
        $this->app->bind(HearthCommands::class, BundledHearth::class);
    }

    /**
     * Bootstrap any application services.
     */
    public function boot(): void
    {
        //
    }
}
