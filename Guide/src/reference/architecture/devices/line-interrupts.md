# Line interrupts

This page explains interrupt-line ownership, sharing, and lifecycle behavior in
the device infrastructure shared by OpenVMM and OpenHCL.

A line interrupt represents a signal level, not a queued interrupt message.
Devices publish high or low through [`LineInterrupt`][line]. A
[`LineSet`][set] combines shared outputs and forwards the resulting wire level
to interrupt targets, such as the PIC, IO APIC, or a power-management GPE latch.
The target decides what that level means for interrupt delivery.

## Ownership and sharing

| Component | Responsibility |
| --- | --- |
| Producer | Own the output level and the device state that determines it. Save and restore enough state to reconstruct that output. |
| Line manager | Track each producer's contribution, combine shared outputs, and gate forwarding across VM lifecycle transitions. |
| Target | Recognize interrupts and preserve the history or pending state needed to interpret input levels correctly. |

A producer receives a handle to its own output, rather than an interface for
asserting arbitrary interrupt-controller inputs. Producers sharing a wire have
separate contributions: the wire is high if any producer drives it high.
Lowering or dropping one producer's output does not lower the wire while
another producer still drives it high. One wire can also feed multiple targets.

There is no separately saved line-manager state. Restore reconstructs the
producer contributions from device state, while targets restore their own
interrupt-recognition state. For example, the IO APIC remembers input levels
to distinguish an already-high input from a new rising edge. That history does
not make the IO APIC the owner of the device's output.

## Establish outputs before start

Establish every output during construction, reset, and restore, before the VM
starts. Publish low explicitly when no interrupt is asserted; do not wait for
the first register access, timer expiration, or device poll.

Restore must reconstruct the output without processing new work. For example,
an IDE channel with a saved pending interrupt must publish its restored level
before polling its drives. Otherwise, replay can temporarily lower a previously
high wire, and the first poll can manufacture a rising edge.

An output is initially unpublished, and line-manager reset marks existing
outputs unpublished again. Ordinary stop/start retains publication validity.
At start, an unpublished source attached to a target produces a warning and
contributes low to the shared wire. This is a diagnostic, not a proof of restore
correctness: publishing low during construction does not excuse failing to
restore a saved high output.

Restore only into newly constructed or successfully reset stopped components.
A paused VM with old device state is not a valid restore destination. See
[Save State](../../../dev_guide/contrib/save-state.md#destination-lifecycle)
for the destination and retry requirements.

## Start, stop, and reset ordering

Start in dependency order:

1. Start the targets so they can respond to input changes.
2. Start the line manager, synchronously replaying every connected wire's final
   level, including low, and enabling subsequent forwarding.
3. Start the producers, allowing them to process new work.

Stop in the reverse order: stop producers, stop line-manager forwarding, then
stop targets. After a producer's stop completes, it must not continue processing
asynchronous work.

A line change before forwarding stops still reaches the target, which can
update its interrupt-recognition state before it is stopped and saved. Once
forwarding stops, publications only update cached producer contributions. They
do not call stopped targets. Replay at the next start presents the final shared
level, not every intermediate transition.

For reset, stop the VM first, reset the line manager before producers reset,
and have producers establish their reset outputs. Targets reset their own
recognition state. Start then uses the same target-manager-producer ordering.

These dependencies also apply when a device is both a target and a producer:
for example, a GPE latch receives device signals and produces SCI, while the
PIC receives IRQ inputs and produces an interrupt-ready output.

## Replay is not a new interrupt event

Consider an IDE channel connected through a line manager to an unmasked,
edge-triggered IO APIC input:

1. The channel has a pending interrupt and drives high. The manager forwards
   high, and the IO APIC recognizes the low-to-high transition.
2. The VM stops and saves the channel's pending state and the IO APIC's
   remembered high input.
3. In a fresh or reset destination, the channel restores its pending state and
   publishes high while forwarding is stopped. The IO APIC restores its high
   input history without delivering an interrupt during restore.
4. At start, the manager replays high. The IO APIC sees high followed by high,
   so this does not manufacture another rising edge.
5. Guest acknowledgment eventually clears the channel's interrupt condition.
   The channel publishes low, which is forwarded normally.

If the saved input history was low but the restored producer intentionally
drives high, replay instead presents a real low-to-high difference. The target
can recognize that edge. A level-triggered target can also reevaluate a replayed
high according to its masking and acknowledgment state; the manager does not
decide whether another interrupt should be delivered.

## Pulses and pending interrupts

The line API does not model a minimum pulse duration or guarantee that a target
will retain a request after the producer lowers its output. While forwarding is
enabled, targets are called synchronously with the shared wire level, but their
own hardware semantics determine whether a pulse is useful.

For example, a generation-ID notification pulses a GPE input on x86. The GPE
target latches status until guest acknowledgment and drives a persistent SCI
output. By contrast, merely selecting edge-triggered mode on the PIC does not
make a short pulse a reliable queued request; its input must remain asserted
through the required acknowledgment phase.

A high-to-low pulse completed while forwarding is stopped is lost. There is
no line-manager pending-event queue, and no pulse is synthesized at resume. A
producer that remains high through resume retains its final contribution, which
the target interprets against its saved history.

Use a persistent output for a persistent interrupt condition, and clear it at
the device's defined acknowledgment or completion event. Do not use a line
pulse as a substitute for message-signaled interrupts unless the receiving
target's latching semantics provide the required behavior.

## Inputs without a producer

An input with no connected producer is different from a connected producer
that has not published a level. This distinction matters when restoring with a
different device topology or removing the last producer from a wire.

Pure line-controller inputs should be deasserted when unconnected. However,
some targets share an interrupt namespace with non-line sources. Initializing
an unused line must not clear independently pending interrupts in that
namespace. The target-specific unconnected-input policy preserves that
distinction.

For API details, including target callback constraints, see the
[`vmcore::line_interrupt` rustdoc][interrupts]. Device lifecycle requirements
are documented on [`ChangeDeviceState`][device-state].

[line]: https://openvmm.dev/rustdoc/linux/vmcore/line_interrupt/struct.LineInterrupt.html
[set]: https://openvmm.dev/rustdoc/linux/vmcore/line_interrupt/struct.LineSet.html
[interrupts]: https://openvmm.dev/rustdoc/linux/vmcore/line_interrupt/index.html
[device-state]: https://openvmm.dev/rustdoc/linux/vmcore/device_state/trait.ChangeDeviceState.html
