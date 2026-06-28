param(
    [string]$Url = "http://127.0.0.1:8080/json",
    [int]$Requests = 10000,
    [int]$Concurrency = 100,
    [int]$TimeoutSeconds = 60
)

$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)

if ($Requests -lt 1) {
    throw "Requests must be >= 1"
}
if ($Concurrency -lt 1 -or $Concurrency -gt 4096) {
    throw "Concurrency must be between 1 and 4096"
}
if ($TimeoutSeconds -lt 1) {
    throw "TimeoutSeconds must be >= 1"
}

$source = @"
using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Net.Http;
using System.Threading;
using System.Threading.Tasks;

public sealed class KuHttpBenchResult
{
    public long StartMs;
    public long FinishMs;
    public long WallMs;
    public double Rps;
    public int Requests;
    public int Concurrency;
    public int Errors;
    public double AvgMs;
    public double P50Ms;
    public double P95Ms;
    public double P99Ms;
    public string StatusCounts = "";
}

public static class KuHttpBench
{
    public static KuHttpBenchResult Run(string url, int requests, int concurrency, int timeoutSeconds)
    {
        var handler = new HttpClientHandler();
        handler.MaxConnectionsPerServer = concurrency;
        using (var client = new HttpClient(handler))
        {
            client.Timeout = TimeSpan.FromSeconds(timeoutSeconds);
            var latencies = new List<double>(requests);
            var statusCounts = new ConcurrentDictionary<int, int>();
            var next = 0;
            var errors = 0;
            var all = Stopwatch.StartNew();
            var startMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
            var workers = Enumerable.Range(0, concurrency).Select(_ => Task.Run(async () =>
            {
                while (true)
                {
                    var id = Interlocked.Increment(ref next);
                    if (id > requests)
                    {
                        break;
                    }
                    var one = Stopwatch.StartNew();
                    try
                    {
                        using (var response = await client.GetAsync(url).ConfigureAwait(false))
                        {
                            await response.Content.ReadAsStringAsync().ConfigureAwait(false);
                            statusCounts.AddOrUpdate((int)response.StatusCode, 1, (statusCode, value) => value + 1);
                        }
                    }
                    catch
                    {
                        Interlocked.Increment(ref errors);
                    }
                    finally
                    {
                        one.Stop();
                        lock (latencies)
                        {
                            latencies.Add(one.Elapsed.TotalMilliseconds);
                        }
                    }
                }
            })).ToArray();
            if (!Task.WaitAll(workers, TimeSpan.FromSeconds(timeoutSeconds + 10)))
            {
                throw new TimeoutException("HTTP benchmark exceeded the external deadline");
            }
            all.Stop();
            var finishMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
            double[] sorted;
            lock (latencies)
            {
                sorted = latencies.OrderBy(value => value).ToArray();
            }
            var count = sorted.Length;
            Func<double, double> percentile = p =>
            {
                if (count == 0) return 0;
                var index = Math.Min(count - 1, (int)Math.Floor(count * p));
                return sorted[index];
            };
            var statusText = string.Join(", ", statusCounts.OrderBy(pair => pair.Key).Select(pair => pair.Key + ":" + pair.Value));
            return new KuHttpBenchResult
            {
                StartMs = startMs,
                FinishMs = finishMs,
                WallMs = (long)Math.Round(all.Elapsed.TotalMilliseconds),
                Rps = all.Elapsed.TotalSeconds > 0 ? requests / all.Elapsed.TotalSeconds : 0,
                Requests = requests,
                Concurrency = concurrency,
                Errors = errors,
                AvgMs = count > 0 ? sorted.Average() : 0,
                P50Ms = percentile(0.50),
                P95Ms = percentile(0.95),
                P99Ms = percentile(0.99),
                StatusCounts = statusText
            };
        }
    }
}
"@

Add-Type -TypeDefinition $source -ReferencedAssemblies "System.Net.Http.dll"
$result = [KuHttpBench]::Run($Url, $Requests, $Concurrency, $TimeoutSeconds)

Write-Host "=== Ku HTTP benchmark ==="
Write-Host ("URL: {0}" -f $Url)
Write-Host ("StartMs: {0}" -f $result.StartMs)
Write-Host ("FinishMs: {0}" -f $result.FinishMs)
Write-Host ("Requests: {0}" -f $result.Requests)
Write-Host ("Concurrency: {0}" -f $result.Concurrency)
Write-Host ("WallMs: {0}" -f $result.WallMs)
Write-Host ("RPS: {0:N2}" -f $result.Rps)
Write-Host ("Errors: {0}" -f $result.Errors)
Write-Host ("LatencyAvgMs: {0:N2}" -f $result.AvgMs)
Write-Host ("LatencyP50Ms: {0:N2}" -f $result.P50Ms)
Write-Host ("LatencyP95Ms: {0:N2}" -f $result.P95Ms)
Write-Host ("LatencyP99Ms: {0:N2}" -f $result.P99Ms)
Write-Host ("StatusCounts: {0}" -f $result.StatusCounts)
