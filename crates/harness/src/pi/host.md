# Heavy work on this machine

You share this computer with the user and with other agent sessions. Do not overload it. Heavy work means full or workspace-wide test suites, release or whole-workspace builds, container builds, benchmarks, starting a workflow, or several sub-agents at once.

Before you start heavy work here, check how busy the machine is:
- Linux: `free -m`, `nproc`, `uptime` (load average), `cat /proc/pressure/memory` when it exists, and `systemctl --user show zeron.service -p MemoryCurrent -p MemoryHigh` when Zeron runs as that unit (your processes share its memory cap).
- macOS: `vm_stat`, `sysctl -n hw.ncpu hw.memsize`, `uptime`.
- Windows (PowerShell): `Get-CimInstance Win32_OperatingSystem | Select-Object FreePhysicalMemory, TotalVisibleMemorySize` and `(Get-Counter '\Processor(_Total)\% Processor Time').CounterSamples.CookedValue`.
- GPU work: `nvidia-smi --query-gpu=utilization.gpu,memory.used,memory.total --format=csv`.

The machine is busy when free memory is under 15% or under 2 GB, the memory cap is above 85% used, memory pressure `some avg10` is above 10, the 1-minute load average is above the core count, or the GPU is above 80% used. If it is busy, do not start the heavy work. Instead, in this order:
1. Run it in CI when the project has CI that covers it: push the branch and watch the run (`gh pr checks --watch`, `gh run watch`).
2. Run a narrower slice: one package, one test file or one test.
3. Wait a minute or two and check again, a few times at most.

Tell the user what you deferred and why, with the numbers you saw. If the user asks you to run it here anyway, do it, and say the machine was busy.

Run one heavy job at a time: no parallel builds, and do not have every sub-agent or workflow agent run the full suite.
