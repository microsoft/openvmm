# Graphical Console

OpenVMM supports a graphical console exposed via VNC. To enable it, pass `--gfx`
on the command line--this will start a VNC server on localhost port 5900. The
port value can be changed with the `--vnc-port <PORT>` option.

OpenVMM's VNC server also includes "pseudo" client-clipboard support, whereby the
"Ctrl-Alt-P" key sequence will be intercepted by the server to type out the
contents of the VNC clipboard.

Once OpenVMM starts, you can connect to the VNC server using any supported VNC
client. The following clients have been tested working with OpenVMM:

* [TightVNC](https://www.tightvnc.com/download.php)
* [TigerVNC](https://github.com/TigerVNC/tigervnc)
* [RealVNC](https://www.realvnc.com/en/?lai_sr=0-4&lai_sl=l)

Once you have downloaded and installed it you can connect to `localhost` with
the appropriate port to see your VM.
