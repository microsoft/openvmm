# Running OpenHCL

This chapter provides a high-level overview of different ways to launch and
interact with OpenHCL.

* * *

To get started, ensure you have a copy of an OpenHCL IGVM firmware image, via
one of the following options:

## Building OpenHCL

Please refer to the page [Building OpenHCL](../../dev_guide/getting_started/build_openhcl.md)

If you are going to run OpenHCL on Windows, once you have built your OpenHCL IGVM image on WSL or Linux following those instructions,  find the .bin file in your WSL or Linux instance.

You can use `find . -name "*.bin"`.  As an example, it could be in `/home/YourWSLUsername/openvmm/flowey-out/artifacts/build-igvm/debug/<RECIPE>/openhcl-<RECIPE>.bin` if using WSL.

Make sure to copy that .bin file to your Windows host somewhere that vmwp.exe has permissions to read it, which can be in windows\system32, or another directory with wide read access.


## Pre-Built Binaries

If you would prefer to try OpenHCL without building it from scratch, you can
download pre-built copies of OpenHCL IGVM files from
[OpenVMM CI](https://github.com/microsoft/openvmm/actions/workflows/openvmm-ci.yaml).

Simply select a successful pipeline run (should have a Green checkbox), and
scroll down to select an appropriate `*-openhcl-igvm` artifact for your
particular architecture and operating system.
