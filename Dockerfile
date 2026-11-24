# Build with ./build.sh. Two targets: `verus` (the pinned verifier) and the sandbox itself.
FROM debian:trixie-slim AS verus
ARG VERUS=0.2026.11.02.8bf68ee
ARG COMMIT=8bf68ee741df833ea5d0fe6a2255126fdea87e47
ARG RUST=1.99.1
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl unzip gcc libc6-dev procps python3-minimal ripgrep \
 && rm -rf /var/lib/apt/lists/*
RUN case "$(uname -m)" in \
      aarch64) A=arm64; SUM=bc28850dd5efd1679a4cf4ffd3309921c3523c69fa4757079e5d12d4122d400d ;; \
      x86_64) A=x86; SUM=f3a0119292df8855e4435da8130bee1873b0e6a60c306accf81a08ea687bba3c ;; \
    esac \
 && curl -fsSL "https://github.com/verus-lang/verus/releases/download/release/$VERUS/verus-$VERUS-$A-linux.zip" -o /tmp/v.zip \
 && echo "$SUM  /tmp/v.zip" | sha256sum -c - \
 && unzip -q /tmp/v.zip -d /opt && mv /opt/verus-* /opt/verus && rm /tmp/v.zip
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:/opt/verus:/usr/local/bin:/usr/bin:/bin
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal -c rust-src --default-toolchain "$RUST"
# Sources for the docs tool. The release has no vstd source.
RUN mkdir /opt/docs \
 && curl -fsSL "https://github.com/verus-lang/verus/archive/$COMMIT.tar.gz" \
    | tar -xz -C /opt/docs --strip-components=2 "verus-$COMMIT/source/vstd" \
 && for c in core alloc std; do ln -s /opt/rustup/toolchains/*/lib/rustlib/src/rust/library/$c /opt/docs/$c; done \
 && chmod -R a+rX /opt && verus --version

FROM verus AS build
COPY Cargo.toml Cargo.lock ./src-build/
COPY src ./src-build/src
RUN cd src-build && cargo build --release --locked

FROM verus
RUN printf 'agent:x:1001:1001::/scratch:/bin/bash\nverifier:x:1002:1002::/verify:/bin/bash\n' >> /etc/passwd
COPY --from=build /src-build/target/release/vsb /usr/local/bin/vsb
