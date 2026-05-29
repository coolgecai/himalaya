FROM rust:bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        git \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

ENV CARGO_TERM_COLOR=always
WORKDIR /workspace
CMD ["bash"]
