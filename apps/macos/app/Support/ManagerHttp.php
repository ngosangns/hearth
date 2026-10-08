<?php

namespace App\Support;

use Illuminate\Http\Client\Response;
use Illuminate\Support\Facades\Http;
use Illuminate\Support\Str;
use Throwable;

/**
 * Bearer client for one project daemon. The token is read from process memory
 * at each call and is never stored on this object after the request returns.
 */
final class ManagerHttp
{
    public function __construct(private string $root) {}

    public static function open(string $root): ?self
    {
        if (DaemonMemory::token($root) === null || DaemonMemory::publicSession($root) === null) {
            return null;
        }

        return new self($root);
    }

    /**
     * @param  array<string, mixed>  $query
     */
    public function get(string $path, array $query = []): ApiResult
    {
        return $this->send('get', $path, $query, null);
    }

    /**
     * @param  array<string, mixed>  $body
     */
    public function submit(string $serviceId, string $action, bool $killUnowned = false): ApiResult
    {
        $body = [
            'requestId' => (string) Str::uuid(),
            'serviceId' => $serviceId,
            'action' => $action,
        ];
        if ($killUnowned) {
            if ($action !== 'start') {
                return new ApiResult(false, null, null, false, 'killUnowned only applies to a start action');
            }
            $body['killUnowned'] = true;
        }

        return $this->send('post', '/v1/operations', [], $body);
    }

    /**
     * Poll until the operation is succeeded or failed. Interval is about 250ms.
     */
    public function wait(string $operationId, int $timeoutSeconds = 180): ApiResult
    {
        $deadline = microtime(true) + $timeoutSeconds;
        $last = null;
        while (microtime(true) < $deadline) {
            $last = $this->get('/v1/operations/'.rawurlencode($operationId));
            if (! $last->ok || $last->unauthorized) {
                return $last;
            }
            $status = $last->json['operation']['status'] ?? null;
            if ($status === 'succeeded' || $status === 'failed') {
                return $last;
            }
            usleep(250_000);
        }

        return new ApiResult(false, $last?->status, $last?->json, false, 'operation timed out');
    }

    public function daemonLog(int $bytes = 32768): ApiResult
    {
        return $this->get('/v1/daemon/log', ['bytes' => $bytes]);
    }

    public function serviceLog(string $serviceId, ?int $cursor, ?int $generation, int $limit = 32768): ApiResult
    {
        $query = ['limit' => $limit];
        if ($cursor !== null) {
            $query['cursor'] = $cursor;
        }
        if ($generation !== null) {
            $query['generation'] = $generation;
        }

        return $this->get('/v1/logs/'.rawurlencode($serviceId), $query);
    }

    /**
     * @param  array<string, mixed>  $query
     * @param  ?array<string, mixed>  $body
     */
    private function send(string $method, string $path, array $query, ?array $body): ApiResult
    {
        $token = DaemonMemory::token($this->root);
        $session = DaemonMemory::publicSession($this->root);
        if ($token === null || $session === null) {
            return ApiResult::unauthorized();
        }

        try {
            $pending = Http::withToken($token)
                ->timeout(20)
                ->acceptJson()
                ->baseUrl('http://127.0.0.1:'.$session['port']);
            // Authorized routes answer 426 unless this matches the daemon protocol.
            $protocol = $session['protocolVersion'] ?? null;
            if (is_int($protocol) || (is_string($protocol) && ctype_digit($protocol))) {
                $pending = $pending->withHeader('x-hearth-protocol', (string) $protocol);
            }
            $response = $method === 'post'
                ? $pending->post($path, $body ?? [])
                : $pending->get($path, $query);
        } catch (Throwable $error) {
            return ApiResult::transport($error->getMessage());
        }

        return $this->interpret($response);
    }

    private function interpret(Response $response): ApiResult
    {
        if ($response->status() === 401) {
            DaemonMemory::forgetRoot($this->root);

            return ApiResult::unauthorized();
        }
        $json = $response->json();
        $decoded = is_array($json) ? $json : null;
        $message = '';
        if (is_array($decoded['error'] ?? null) && is_string($decoded['error']['message'] ?? null)) {
            $message = $decoded['error']['message'];
        }

        return new ApiResult($response->successful(), $response->status(), $decoded, false, $message);
    }
}
