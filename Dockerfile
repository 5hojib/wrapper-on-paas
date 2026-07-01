# wrapper-v2 image.
#
# This Dockerfile is optimized for deployment on Render and other non-root
# environments. It can automatically download Apple Music libraries if APK_URL
# is provided.

ARG RUNTIME_PLATFORM=linux/amd64
# Kept for docker-compose compatibility; the compile stage ignores this.
ARG BUILD_PLATFORM=linux/amd64

# -----------------------------------------------------------------------------
# Build stage (always amd64 — official NDK is x86_64-hosted)
# -----------------------------------------------------------------------------
FROM --platform=linux/amd64 debian:13.2 AS build

ARG TARGET_ARCH=x86_64
ARG CMAKE_BUILD_TYPE=Release
ARG NDK_VERSION=23
ARG APK_URL="https://github.com/5hojib/WealthWise/releases/download/v1/apple-music.apkm"

SHELL ["/bin/bash", "-c"]
ENV DEBIAN_FRONTEND=noninteractive

RUN --mount=type=cache,target=/var/lib/apt,sharing=locked \
    --mount=type=cache,target=/var/cache/apt,sharing=locked \
    apt-get update && \
    apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        curl \
        git \
        jq \
        ninja-build \
        unzip \
    && if [[ "$TARGET_ARCH" == "arm64-v8a" ]]; then \
         apt-get install -y --no-install-recommends \
           gcc-aarch64-linux-gnu g++-aarch64-linux-gnu; \
       fi \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fSL -o /tmp/ndk.zip \
        "https://dl.google.com/android/repository/android-ndk-r${NDK_VERSION}b-linux.zip" && \
    unzip -q /tmp/ndk.zip -d /opt && \
    rm /tmp/ndk.zip
ENV ANDROID_NDK_HOME=/opt/android-ndk-r${NDK_VERSION}b

WORKDIR /app
COPY . /app

# Stage system libraries first.
RUN bash tools/stage-system.sh --arch "$TARGET_ARCH"

# Extract Apple Music libraries if URL is provided, otherwise expect them to be staged.
RUN if [ -n "$APK_URL" ]; then \
        echo "Downloading Apple Music from $APK_URL..." && \
        curl -fSL -o apple_music.apkm "$APK_URL" && \
        bash tools/extract-libs.sh --bundle apple_music.apkm --arch "$TARGET_ARCH" && \
        rm apple_music.apkm; \
    fi

RUN test -f rootfs/system/bin/linker64 || { \
        echo "ERROR: rootfs/system/bin/linker64 is missing." >&2; \
        exit 1; \
    } && \
    test -d rootfs/system/lib64 && \
    ls rootfs/system/lib64/*.so >/dev/null 2>&1 || { \
        echo "ERROR: rootfs/system/lib64/ has no .so files." >&2; \
        echo "Stage the required Apple Music native libraries or provide APK_URL." >&2; \
        exit 1; \
    }

RUN host_cc=(); \
    if [[ "$TARGET_ARCH" == "arm64-v8a" ]]; then \
      host_cc=( \
        -DCMAKE_C_COMPILER=aarch64-linux-gnu-gcc \
        -DCMAKE_CXX_COMPILER=aarch64-linux-gnu-g++ \
      ); \
    fi && \
    cmake -S . -B build -G Ninja \
        -DTARGET_ARCH="${TARGET_ARCH}" \
        "${host_cc[@]}" \
        -DCMAKE_BUILD_TYPE="${CMAKE_BUILD_TYPE}" && \
    cmake --build build -j

# -----------------------------------------------------------------------------
# Runtime stage
# -----------------------------------------------------------------------------
FROM --platform=${RUNTIME_PLATFORM} debian:13.2

# Install ca-certificates for SSL verification.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

# Create a non-root user.
RUN useradd -m -u 1000 wrapper && \
    mkdir -p /data/data/com.apple.android.music/files && \
    chown -R 1000:0 /data && \
    chmod -R g=u /data

# Copy the built daemon and the staged rootfs to the real root.
# This allows the Android linker to find everything without chroot.
COPY --from=build /app/rootfs/system /system
COPY --from=build /app/rootfs/system/bin/main /usr/local/bin/wrapper-daemon

# Point Bionic at the right places.
ENV ANDROID_DATA=/data
ENV ANDROID_ROOT=/system
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
ENV CURL_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt
ENV ANDROID_DNS_MODE=local

# Default port for Render.
ENV PORT=8080
EXPOSE 8080

USER wrapper
WORKDIR /home/wrapper

# Start the daemon via the Android linker.
ENTRYPOINT ["/system/bin/linker64", "/usr/local/bin/wrapper-daemon"]
