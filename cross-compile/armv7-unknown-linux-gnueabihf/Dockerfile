FROM rustembedded/cross:armv7-unknown-linux-gnueabihf
MAINTAINER phuongnn "nhatphuong0108@gmail.com"

RUN echo "deb http://ftp.de.debian.org/debian stretch main" >> /etc/apt/sources.list
RUN dpkg --add-architecture armhf && \
    apt-get update && \
    apt-get install -y \
        libavcodec-dev:armhf \
        libavformat-dev:armhf \
        libavutil-dev:armhf \
        libncurses5-dev:armhf \
        libncursesw5-dev:armhf \
        libsqlite3-dev:armhf \
        libssl-dev:armhf \
        pkgconf \
        sqlite3:armhf \

RUN ln -s /usr/include/arm-linux-gnueabihf/openssl/opensslconf.h /usr/include/openssl/opensslconf.h

ENV OPENSSL_LIB_DIR=/usr/lib/arm-linux-gnueabihf/ \
    ARMV7_UNKNOWN_LINUX_GNUEABIHF_OPENSSL_LIB_DIR=/usr/lib/arm-linux-gnueabihf/ \
    OPENSSL_INCLUDE_DIR=/usr/include/ \
    ARMV7_UNKNOWN_LINUX_GNUEABIHF_OPENSSL_INCLUDE_DIR=/usr/include/ \
    PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_PATH=/usr/lib/arm-linux-gnueabihf/pkgconfig \
    PKG_CONFIG_SYSROOT_DIR=/

CMD	[ "/bin/bash" ]

