#!/bin/sh
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -eu

trace=/sys/kernel/tracing
if [ ! -f "$trace/tracing_on" ]; then
    mount -t tracefs tracefs "$trace"
fi

case "$(cat "$trace/trace_clock")" in
    *"[mono]"*) ;;
    *) echo "Startup trace is not using the common monotonic clock" >&2; exit 1 ;;
esac
for event in sched/sched_switch sched/sched_wakeup \
    hyperv/mshv_vtl_enter_vtl0 hyperv/mshv_vtl_exit_vtl0 \
    uart_console/record uart_console/wait uart_console/slow_write; do
    test "$(cat "$trace/events/$event/enable")" = 1
done
test "$(cat "$trace/tracing_on")" = 1

echo '=== KERNEL ==='
uname -a
cat /etc/kernel-build-info.json
echo '=== COMMAND LINE ==='
cat /proc/cmdline
echo '=== CLOCK ==='
cat "$trace/trace_clock"

# Consume while recording so boot-time events are not overwritten.
echo '=== TRACE ==='
cat "$trace/trace_pipe" &
reader=$!
trap 'kill "$reader"' EXIT
sleep 15
echo 0 > "$trace/tracing_on"
if wait "$reader"; then
    trap - EXIT
else
    status=$?
    trap - EXIT
    exit "$status"
fi

echo '=== TRACE STATISTICS ==='
for stats in "$trace"/per_cpu/cpu*/stats; do
    echo "$stats"
    cat "$stats"
done
echo '=== INTERRUPTS ==='
cat /proc/interrupts

lost=$(awk '/^overrun:|^commit overrun:|^dropped events:/ { n += $NF } END { print n + 0 }' \
    "$trace"/per_cpu/cpu*/stats)
if [ "$lost" -ne 0 ]; then
    echo "Startup trace lost $lost events" >&2
    exit 1
fi
