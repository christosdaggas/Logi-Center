// SPDX-License-Identifier: GPL-2.0-or-later
import Gio from 'gi://Gio';
import Shell from 'gi://Shell';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const INTERFACE = `
<node>
  <interface name="org.gnome.Shell.Extensions.LogiCenter">
    <method name="GetForeground">
      <arg type="s" name="identity" direction="out"/>
    </method>
  </interface>
</node>`;

export default class LogiCenterContext extends Extension {
    enable() {
        this._export = Gio.DBusExportedObject.wrapJSObject(INTERFACE, this);
        this._export.export(Gio.DBus.session, '/org/gnome/Shell/Extensions/LogiCenter');
    }

    GetForeground() {
        const window = global.display.focus_window;
        if (!window)
            return JSON.stringify({app_id: '', wm_class: '', pid: 0});
        const app = Shell.WindowTracker.get_default().get_window_app(window);
        return JSON.stringify({
            app_id: app?.get_id() ?? '',
            wm_class: window.get_wm_class() ?? '',
            pid: window.get_pid(),
        });
    }

    disable() {
        this._export?.unexport();
        this._export = null;
    }
}
