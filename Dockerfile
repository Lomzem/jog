FROM ubuntu:24.04 AS build

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/opt/cargo \
    RUSTUP_HOME=/opt/rustup \
    PATH=/opt/cargo/bin:$PATH

RUN apt-get update \
    && apt-get install --no-install-recommends -y \
        binutils \
        binutils-mingw-w64-x86-64 \
        ca-certificates \
        curl \
        gcc \
        gcc-mingw-w64-x86-64 \
        libc6-dev \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain 1.97.1 \
    && rustup target add x86_64-pc-windows-gnu

WORKDIR /work
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN cargo fetch --locked

COPY README.md ./
ARG PACKAGE_VERSION
RUN test -n "$PACKAGE_VERSION" \
    && cargo test --release --locked --target x86_64-unknown-linux-gnu \
    && cargo build --release --locked --target x86_64-unknown-linux-gnu \
    && cargo test --release --locked --target x86_64-pc-windows-gnu --no-run \
    && cargo build --release --locked --target x86_64-pc-windows-gnu \
    && install -Dm0755 target/x86_64-unknown-linux-gnu/release/jog /package/usr/bin/jog \
    && strip /package/usr/bin/jog \
    && /package/usr/bin/jog --version \
    && install -Dm0755 /package/usr/bin/jog "/out/jog-${PACKAGE_VERSION}-linux-x86_64" \
    && install -Dm0644 README.md /package/usr/share/doc/jog/README.md \
    && install -Dm0755 target/x86_64-pc-windows-gnu/release/jog.exe "/out/jog-${PACKAGE_VERSION}-windows-x86_64.exe" \
    && x86_64-w64-mingw32-strip "/out/jog-${PACKAGE_VERSION}-windows-x86_64.exe" \
    && x86_64-w64-mingw32-objdump -f "/out/jog-${PACKAGE_VERSION}-windows-x86_64.exe" \
    && mkdir -p /package/DEBIAN \
    && printf '%s\n' \
        'Package: jog' \
        "Version: $PACKAGE_VERSION" \
        'Section: devel' \
        'Priority: optional' \
        'Architecture: amd64' \
        'Maintainer: jog maintainers <maintainers@example.invalid>' \
        'Depends: openocd' \
        'Description: ATSAM4E8C CLI for an Atmel-ICE' \
        > /package/DEBIAN/control \
    && dpkg-deb --build --root-owner-group /package "/out/jog_${PACKAGE_VERSION}_amd64.deb"

FROM scratch AS artifacts
COPY --from=build /out/ /
