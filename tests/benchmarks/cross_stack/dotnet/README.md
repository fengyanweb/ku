# .NET full-stack comparison

.NET10 LTS SDK10.0.402/runtime10.0.13, Kestrel Minimal API,
System.Text.Json dynamic `JsonNode`, Npgsql10.0.3. `global.json` disables SDK
roll-forward and `packages.lock.json` pins the full NuGet graph including hashes;
build uses `restore --locked-mode`, Release, normal tiered JIT/server GC.

All Kestrel, CPU workers and GC threads share one service cgroup. GC heap budget
is65% (`DOTNET_GCHeapHardLimitPercent=41`, hexadecimal), not a process hard cap;
1GiB/256MiB total memory is independently enforced by the harness. Processor
count follows the resource domain. CPU executor has cores consumers/128 queued.
JSON decoding creates a real dynamic tree; the data node transfers from the
no-longer-used decoded envelope into the new output, then STJ encodes it afresh.
No cached body/raw byte echo. Pool maximum16 is total, no automatic SQL retry or
prepared statement cache, normal Npgsql session-reset behavior remains enabled.
This is distinct from Ku's `DISCARD ALL` reset and is disclosed in PG comparisons.

`dotnet Ku.CrossStack.dll --batch INPUT` uses the same bounded integer recurrence.
Startup/JIT/GC remain in full process accounting; internal compute uses Stopwatch.

Official sources checked 2026-10-23:
- https://dotnet.microsoft.com/en-us/platform/support/policy/dotnet-core
- https://builds.dotnet.microsoft.com/dotnet/release-metadata/10.0/releases.json
- https://www.npgsql.org/doc/release-notes/10.0.html

Scope is HTTP/JSON/mixed (PG+timer), batch; not four-stack TCP waiting. Metrics
report actual counters and standard GC cumulative values, not precise per-request
allocation attribution.
