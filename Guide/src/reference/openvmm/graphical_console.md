# Graphical Console

OpenVMM supports a graphical console exposed via VNC. To enable it, pass `--gfx`
on the command line--this will start a VNC server on localhost port 5900. The
port value can be changed with the `--vnc-port <PORT>` option.

OpenVMM's VNC server also includes "pseudo" client-clipboard support, whereby the
"Ctrl-Alt-P" key sequence will be intercepted by the server to type out the
contents of the VNC clipboard.

The VNC server supports RFB protocol versions 3.3, 3.7, and 3.8, with no
authentication (security type "None"). It negotiates the following optional
features based on client capabilities:

* **Zlib compression** (encoding type 6) -- when the client advertises support,
  tile data is zlib-compressed to reduce bandwidth.
* **Cursor pseudo-encoding** -- a local arrow cursor is rendered client-side when
  supported, eliminating server-side cursor compositing.
* **DesktopSize pseudo-encoding** -- resolution changes are relayed to the client.
  Clients that do not advertise this encoding will be disconnected on a
  resolution change.
* **QEMU extended key events** -- when available, the server uses scancode-based
  key input instead of xkeysym translation.
* **Client reconnection** -- a new VNC client connecting will cleanly disconnect
  the previous session and take over.

Once OpenVMM starts, you can connect to the VNC server using any supported VNC
client. The following clients have been tested working with OpenVMM:

* [TightVNC](https://www.tightvnc.com/download.php)
* [TigerVNC](https://github.com/TigerVNC/tigervnc)
* [RealVNC](https://www.realvnc.com/en/?lai_sr=0-4&lai_sl=l)
* [noVNC](https://novnc.com/) (browser-based)

Once you have downloaded and installed it you can connect to `localhost` with
the appropriate port to see your VM.
