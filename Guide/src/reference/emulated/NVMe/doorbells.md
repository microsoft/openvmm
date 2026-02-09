# Doorbells
The doorbell notification system in the NVMe emulator is built around two core structures: `DoorbellMemory` and `DoorbellState`. These components work together to coordinate doorbell updates between the guest and the device, following a server-client like model.

![Figure that shows the basic layout of the doorbell memory and doorbell state. There is 1 doorbell memory struct containing a vector of registered wakers and a pointer in to guest memory at "offset". There are 3 doorbell state structs that each track a different doorbell but all have pointers to the doorbell memory struct](images/Doorbell%20Setup.png "Doorbell Setup")
Fig: Basic layout of DoorbellMemory and DoorbellStates.

## Doorbell Memory

DoorbellMemory serves as the central authority for managing doorbell values. It maintains:

- A reference to guest memory, where doorbell registers are mapped.
- A vector of wakers—one per doorbell—to notify tasks when a doorbell is updated.

The emulator creates this struct upon instantiation. All queues receive a reference to the entire list of doorbells. Doorbell writes are handled by the emulator’s PCI interface, which forwards the writes to DoorbellMemory.


## Doorbell State

DoorbellState is used by tasks that need to track the value of a specific doorbell. In the NVMe emulator, the `SubmissionQueue::tail` and `CompletionQueue::head` are each stored as a DoorbellState. This struct abstracts interactions with DoorbellMemory and synchronization. Tasks can poll the doorbell state. `Poll::Ready` is returned if the doorbell value was updated. 


## Waker functionality

To conserve system resources, DoorbellMemory and DoorbellState use wakers to reduce busy-polling when there is no additional work/updates to the doorbell. Instead of repeatedly polling a future value of the corresponding doorbell, when DoorbellState notices no change to the doorbell value, it registers a waker DoorbellMemory and stops polling. When a doorbell write to bar0 is triggered by the guest, DoorbellMemory will write the new value and trigger the corresponding waker, at which point DoorbellState can poll() again and is guaranteed a change to the value (i.e. more work to go do)


![Figure that shows how the wakers behave when trying to wake up a queue that is awaiting a changed state of a doorbell](images/Doorbell%20Waker.png "Doorbell Waker Flow")
Fig: When a doorbell write comes in and there is a waiting queue, the waker is triggered by DoorbellMemory if there is a registered waker.
