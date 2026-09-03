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
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain 1.97.1 \
    && rustup target add x86_64-pc-windows-gnu

WORKDIR /work
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo fetch --locked

COPY README.md ./
ARG PACKAGE_VERSION
RUN test -n "$PACKAGE_VERSION" \
    && cargo test --release --locked --target x86_64-unknown-linux-gnu \
    && cargo test --release --locked --target x86_64-pc-windows-gnu --no-run \
    && cargo build --release --locked --target x86_64-pc-windows-gnu \
    && install -Dm0755 target/x86_64-unknown-linux-gnu/release/sam4e /package/usr/bin/sam4e \
    && strip /package/usr/bin/sam4e \
    && /package/usr/bin/sam4e --version \
    && install -Dm0644 README.md /package/usr/share/doc/sam4e/README.md \
    && install -Dm0755 target/x86_64-pc-windows-gnu/release/sam4e.exe "/out/sam4e-${PACKAGE_VERSION}-windows-x86_64.exe" \
    && x86_64-w64-mingw32-strip "/out/sam4e-${PACKAGE_VERSION}-windows-x86_64.exe" \
    && x86_64-w64-mingw32-objdump -f "/out/sam4e-${PACKAGE_VERSION}-windows-x86_64.exe" \
    && mkdir -p /package/DEBIAN \
    && printf '%s\n' \
        'Package: sam4e' \
        "Version: $PACKAGE_VERSION" \
        'Section: devel' \
        'Priority: optional' \
        'Architecture: amd64' \
        'Maintainer: sam4e maintainers <maintainers@example.invalid>' \
        'Depends: openocd' \
        'Description: ATSAM4E8C CLI for an Atmel-ICE' \
        > /package/DEBIAN/control \
    && dpkg-deb --build --root-owner-group /package "/out/sam4e_${PACKAGE_VERSION}_amd64.deb"

FROM scratch AS artifacts
COPY --from=build /out/ /
