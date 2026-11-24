FROM debian:bookworm-slim AS verus
ARG VERUS=0.2026.11.02.8bf68ee
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl unzip gcc libc6-dev procps ripgrep && rm -rf /var/lib/apt/lists/*
RUN case "$(uname -m)" in \
      aarch64) A=arm64; SUM=bc28850dd5efd1679a4cf4ffd3309921c3523c69fa4757079e5d12d4122d400d ;; \
      x86_64) A=x86; SUM=f3a0119292df8855e4435da8130bee1873b0e6a60c306accf81a08ea687bba3c ;; \
    esac \
 && curl -fsSL "https://github.com/verus-lang/verus/releases/download/release/$VERUS/verus-$VERUS-$A-linux.zip" -o /tmp/v.zip \
 && echo "$SUM  /tmp/v.zip" | sha256sum -c - \
 && unzip -q /tmp/v.zip -d /opt && mv /opt/verus-* /opt/verus && rm /tmp/v.zip && cat /opt/verus/version.txt
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:/opt/verus:/usr/local/bin:/usr/bin:/bin
RUN TC="$(grep -o '[0-9]*\.[0-9]*\.[0-9]*-[a-z0-9_]*-unknown-linux-gnu' /opt/verus/version.txt | head -1 | grep -o '^[0-9.]*')" \
 && curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal -c rust-src --default-toolchain "$TC" \
 && chmod -R a+rX /opt && verus --version
