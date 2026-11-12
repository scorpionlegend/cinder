Name:           cinder
Version:        0.1.0
Release:        1%{?dist}
Summary:        Cinder web browser
License:        Apache-2.0
URL:            https://github.com/scorpionlegend/cinder
# The payload is the portable Linux bundle produced by `cinder-dist`.
Source0:        %{name}-%{version}-%{_target_cpu}.tar.gz
AutoReqProv:    no
BuildArch:      %{_cinder_arch}

%description
Cinder is a web browser built on the Chromium engine through CEF. The
components Cinder owns are written in Rust; the bundled Chromium runtime is not.

%prep
%setup -q -n %{_cinder_root}

%install
mkdir -p %{buildroot}%{_libdir}/cinder
cp -a . %{buildroot}%{_libdir}/cinder/
mkdir -p %{buildroot}%{_bindir}
ln -s ../lib/cinder/cinder %{buildroot}%{_bindir}/cinder
mkdir -p %{buildroot}%{_datadir}/applications
install -m 0644 %{_cinder_desktop} %{buildroot}%{_datadir}/applications/cinder.desktop
mkdir -p %{buildroot}%{_datadir}/metainfo
install -m 0644 %{_cinder_metainfo} %{buildroot}%{_datadir}/metainfo/io.cinder.Cinder.metainfo.xml

%files
%{_libdir}/cinder
%{_bindir}/cinder
%{_datadir}/applications/cinder.desktop
%{_datadir}/metainfo/io.cinder.Cinder.metainfo.xml

%changelog
* Thu Nov 12 2026 Allen Schoepflin <alband3000@gmail.com> - 0.1.0-1
- Initial package
