# Optional GNOME application identity extension

GNOME Wayland intentionally does not expose every application's foreground
window through X11 tools. This small **read-only** extension exposes the focused
application's desktop ID, WM class and PID over the session bus. It does not read
window titles, capture keyboard input, execute commands or change focus.

Installation is optional and never performed by Logi Center. From this repository:

```sh
gnome-extensions pack assets/gnome-extension --out-dir /tmp
gnome-extensions install /tmp/logicenter-context@chrisdaggas.com.shell-extension.zip
```

Log out and back in if GNOME requires it, then enable the extension in the
Extensions application or with:

```sh
gnome-extensions enable logicenter-context@chrisdaggas.com
```

If a previous version is installed, inspect it before choosing to replace it
with `gnome-extensions install --force`. Declared Shell versions are API targets,
not a claim that every version has been runtime-tested.

The application checks this method without installing or enabling anything:

```sh
gdbus call --session --dest org.gnome.Shell \
  --object-path /org/gnome/Shell/Extensions/LogiCenter \
  --method org.gnome.Shell.Extensions.LogiCenter.GetForeground
```

Without an available foreground backend, manual profile selection still works.
Any process in the same desktop session can query this interface, as it can
other exported session-bus interfaces. Disable the extension to stop exporting it.
