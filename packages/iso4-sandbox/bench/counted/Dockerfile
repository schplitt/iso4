# Build and run image for the counted tier: Debian bookworm, clang 21 (V8's
# libc++ headers need it, as in CI), Rust stable, Node 26 (mise.toml).
FROM node:26-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl lsb-release wget gnupg software-properties-common \
      procps python3 build-essential pkg-config \
 && curl -fsSL --retry 5 https://apt.llvm.org/llvm.sh -o /tmp/llvm.sh \
 && bash /tmp/llvm.sh 21 \
 && rm -rf /var/lib/apt/lists/* /tmp/llvm.sh
ENV PATH=/usr/lib/llvm-21/bin:$PATH

ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
ENV PATH=/usr/local/cargo/bin:$PATH
RUN curl -fsSL --retry 5 https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable \
 && rustc --version
