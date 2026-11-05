# Arch (like Omarchy) build image for the Linux host.
FROM archlinux:latest
# pacman's sandbox can't apply seccomp under emulated x86 builds.
RUN sed -i 's/^\[options\]/[options]\nDisableSandbox/' /etc/pacman.conf \
 && pacman -Syu --noconfirm --needed base-devel clang pipewire libpipewire gstreamer gst-plugins-base-libs rustup pkgconf nasm git \
 && rustup default stable \
 && pacman -Scc --noconfirm
WORKDIR /src
