<?php

use App\Http\Controllers\SpikeController;
use App\Livewire\WorkspaceDesk;
use Illuminate\Support\Facades\Route;

Route::get('/', WorkspaceDesk::class);
Route::get('/spike', [SpikeController::class, 'show']);
Route::post('/spike/ensure', [SpikeController::class, 'ensure'])->name('spike.ensure');
