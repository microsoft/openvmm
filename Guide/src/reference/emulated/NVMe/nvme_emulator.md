# Nvme Emulator
*Brief overview of the emulator and where it is being used*

## Component Overview
*This should be a high level architecture diagram followed by a brief description of each component like the worker, coordinator, admin handler, io handler etc*

## Pci Interface
*This section will talk about how/what pci capabilites are exposed by the nvme emulator, how they are used and how how the enable/disable workflow functions within the device. Also things like the reset flow and error cases.*

### Register Interface
*Talk about the registers and proper configuration*

### Error Handling
*What happens when there is a failure*

### Limitations / Future Work
*Things like FLR support and other improvements should go here*

## Queue Management
Queues are the core component that NVMe leverages to provide fast parallel compute.

### Queue Architecture
*A brief overfiew of the queues structs (both submission and completion)*

### Doorbell Mechanism
*Crucial to understanding how the queues make progress*

#### Shadow Doorbell Support
*Shadow doorbell support recently improved. Talk about that here*

### Admin Queues
*Kind of a special case of the queues, touch upon any nuances here and how any specific handling is working / limitations and unsupported commands for now (there are plenty of unsupported commands rn)*

### Io Queues
*Setup i.e. Multiple Sq support per cqs and how that is working*

#### Creation
#### Deletion

## Namespace Management
*How to create / remove namespaces without using admin commands. There are some test specific changes here*


