# Performance analysis

Besides performance analysis for VTL0 VM and host OS, we'd like to capture trace
inside VTL2 as well. This allows us to analyze the Linux kernel and
`openvmm_hcl` worker process inside VTL2.

## Prerequisite for capturing perf data

The release Linux image doesn't have user mode perf program by default.
To capture perf data inside VTL2, it is necessary to build OpenHCL with the
below instructions, which will include the necessary programs.

```bash
cargo xflowey build-igvm [RECIPE] --release --with-perf-tools
```

### Increase VTL2 memory page count

To capture and save perf data file inside VTL2, we need to increase VTL2's
memory page count by increasing the memory_page_count value in the IGVM
configuration file (openhcl-x64-dev.json). E.g. increase it to 524288 like:

```json
{
    "image": {
        "openhcl": {
            ....................
            "memory_page_count": 524288
        }
    }
}
```

## Capture perf data inside VTL2

While workloads run inside VTL0 VM, use ohcldiag-dev.exe to run perf to capture
perf_events profiling data inside VTL2. E.g. the following command captures 15s
of perf data and saves it in a file. You can find more info about perf on
external page [perf Examples](https://www.brendangregg.com/perf.html)

```bash
.\ohcldiag-dev.exe <VM Name> run -- perf record -F 999 -a --call-graph=dwarf -o ./openhcl.fio.perf -- sleep 15
```

Use perf to convert perf data to plain text and dump it to a file on host

```bash
.\ohcldiag-dev.exe <VM Name> run -- perf script -i ./openhcl.fio.perf > .\traces\openhcl.fio.perf.script
```

## Visualize perf profiling data

Please follow up instructions on the external page [Flame
Graphs](https://www.brendangregg.com/perf.html#FlameGraphs) to create flame
graph SVG file. It requires scripts from FlameGraph GitHub repo, so it is better
to do it on WSL2.

Here is an example command on WSL2. It converts the perf script file to SVG
file. Both of files are located under d:\tmp\vtl2 folder on Windows.

```bash
perf_file=/mnt/d/tmp/vtl2/openhcl.fio.perf.script; cat $perf_file | ./stackcollapse-perf.pl > $perf_file.folded; cat $perf_file.folded | ./flamegraph.pl > $perf_file.svg
```

If the perf script file doesn't have rust functions demangled correctly, please add rustfilt in the pipe to assist demangling.

```bash
perf_file=/mnt/d/tmp/vtl2/openhcl.fio.perf.script; cat $perf_file | rustfilt | ./stackcollapse-perf.pl > $perf_file.folded; cat $perf_file.folded | ./flamegraph.pl > $perf_file.svg
```
