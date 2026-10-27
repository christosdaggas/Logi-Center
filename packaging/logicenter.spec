Name:           logicenter
Version:        %{app_version}
Release:        1%{?dist}
Summary:        A modern Logitech device manager for Fedora and GNOME
License:        GPL-2.0-or-later
URL:            https://chrisdaggas.com
Packager:       Christos A. Daggas <info@chrisdaggas.com>
Source0:        logicenter-%{version}.tar.gz
Requires:       gtk4 >= 4.16
Requires:       libadwaita >= 1.6
Requires:       curl
Requires:       hicolor-icon-theme
Requires:       librsvg2
Requires(post): systemd-udev
Requires(postun): systemd-udev
%global debug_package %{nil}

%description
Manage Logitech mice, keyboards and USB cameras, check batteries, and change
supported settings. Save per-device preferences and DPI presets, use live camera
sliders, and optionally assign shortcuts and gestures to supported buttons.
Made for Fedora with the GNOME desktop.

%prep
%setup -q

%build
# scripts/build-rpm.sh builds the release executable before creating Source0.

%install
install -Dpm0755 logicenter %{buildroot}%{_bindir}/logicenter
install -Dpm0644 com.chrisdaggas.logicenter.desktop %{buildroot}%{_datadir}/applications/com.chrisdaggas.logicenter.desktop
install -Dpm0644 logo.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/com.chrisdaggas.logicenter.svg
install -Dpm0644 symbolic.svg %{buildroot}%{_datadir}/icons/hicolor/symbolic/apps/com.chrisdaggas.logicenter-symbolic.svg
install -Dpm0644 70-logicenter.rules %{buildroot}%{_udevrulesdir}/70-logicenter.rules

%post
udevadm control --reload-rules >/dev/null 2>&1 || :

%postun
udevadm control --reload-rules >/dev/null 2>&1 || :

%files
%license LICENSE
%doc README.md CHANGELOG.md
%{_bindir}/logicenter
%{_datadir}/applications/com.chrisdaggas.logicenter.desktop
%{_datadir}/icons/hicolor/scalable/apps/com.chrisdaggas.logicenter.svg
%{_datadir}/icons/hicolor/symbolic/apps/com.chrisdaggas.logicenter-symbolic.svg
%{_udevrulesdir}/70-logicenter.rules
