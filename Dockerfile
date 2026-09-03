# The computer is a vanilla Alpine, a handful of distro packages, an X server
# we build, and one binary. The binary is PID 1, the window manager, and the
# MCP server; Xvfb owns the pixels; dbus carries the accessibility tree;
# Chromium is the browser.
FROM rust:1-alpine AS build

# rustls's default crypto provider builds aws-lc, which wants cmake and perl.
RUN apk add --no-cache build-base cmake perl
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release --locked


# Alpine's Xvfb links libGL at load time, and libGL drags in Mesa's LLVM
# rasterizer: 300 MB that a framebuffer in RAM never calls. So the X server
# is built here from the xorg-server release, GLX and every hardware path
# off, and links nothing the runtime image does not already carry.
FROM alpine:3.22@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce AS xserver

ARG XSERVER_VERSION=21.1.24
ARG XSERVER_SHA256=1a4eb36ca65cc3b1b936566d677a9786e13c11cd5806e951ac55f3f5ce3984af
RUN apk add --no-cache \
        gcc musl-dev meson ninja-build pkgconf xz \
        xorgproto xtrans libxfont2-dev libxcvt-dev libxkbfile-dev \
        pixman-dev libxau-dev libxdmcp-dev libmd-dev
WORKDIR /src
RUN wget -q -O xorg-server.tar.xz \
        "https://www.x.org/archive/individual/xserver/xorg-server-${XSERVER_VERSION}.tar.xz" \
    && echo "${XSERVER_SHA256}  xorg-server.tar.xz" | sha256sum -c - \
    && tar -xJf xorg-server.tar.xz --strip-components=1 \
    && meson setup build --prefix=/usr --buildtype=release \
        -Dxvfb=true -Dxorg=false -Dxnest=false -Dxephyr=false -Dxwin=false -Dxquartz=false \
        -Dglx=false -Dglamor=false -Ddri1=false -Ddri2=false -Ddri3=false \
        -Dxdmcp=false -Dsecure-rpc=false -Dlisten_tcp=false \
        -Dudev=false -Dudev_kms=false -Dhal=false -Dsystemd_logind=false \
        -Dpciaccess=false -Dint10=false -Dvgahw=false -Ddga=false \
        -Dxv=false -Dxvmc=false -Dxselinux=false \
        -Ddocs=false -Ddevel-docs=false -Ddocs-pdf=false \
        -Dsha1=libmd -Ddefault_font_path=built-ins \
        -Dxkb_dir=/usr/share/X11/xkb -Dxkb_bin_dir=/usr/bin -Dxkb_output_dir=/tmp \
    && ninja -C build hw/vfb/Xvfb \
    && strip build/hw/vfb/Xvfb

FROM alpine:3.22@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce

# What Xvfb links, then the desktop. Chromium's package still names Mesa as
# a dependency for libgbm, whose backends load on demand, so the rasterizer
# files are deleted after install. Input is XTEST and the clipboard is a
# selection the agent owns, so no input or clipboard program is installed.
RUN apk add --no-cache \
        libxfont2 libxcvt libxau libmd pixman xkbcomp xkeyboard-config \
        dbus \
        at-spi2-core \
        chromium \
        ttf-dejavu \
        font-noto-emoji \
    && rm -rf /usr/lib/libLLVM* /usr/lib/libgallium* /usr/lib/gallium-pipe /usr/lib/libGL.so* \
    && adduser -D -u 1000 -h /home/agent agent \
    && install -d -m 1777 /tmp/.X11-unix

COPY --from=xserver /src/build/hw/vfb/Xvfb /usr/bin/Xvfb
COPY --from=build /src/target/release/toad-computer /usr/bin/toad-computer

# Nothing in the container runs as root, so every capability stays dropped.
# Chromium exports its accessibility tree only when ACCESSIBILITY_ENABLED
# says so; without it `capture` sees a window with nothing inside.
USER agent
WORKDIR /home/agent
ENV TOAD_COMPUTER_ADDR=0.0.0.0:8787 \
    TOAD_COMPUTER_HOME=/home/agent \
    TOAD_COMPUTER_SCREEN=1920x1080 \
    DISPLAY=:0 \
    ACCESSIBILITY_ENABLED=1

EXPOSE 8787
ENTRYPOINT ["/usr/bin/toad-computer", "boot"]
