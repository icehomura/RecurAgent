# ============================================================
# Stage 1: Build the ra binary
# ============================================================
FROM rust:1.88-alpine AS builder

RUN apk add --no-cache musl-dev git pkgconfig

WORKDIR /src

# Cache dependencies
COPY Cargo.toml Cargo.lock ./
COPY crates/ra-core/Cargo.toml crates/ra-core/Cargo.toml
COPY crates/ra-llm/Cargo.toml crates/ra-llm/Cargo.toml
COPY crates/ra-memory/Cargo.toml crates/ra-memory/Cargo.toml
COPY crates/ra-agent/Cargo.toml crates/ra-agent/Cargo.toml
COPY crates/ra-bus/Cargo.toml crates/ra-bus/Cargo.toml
COPY crates/ra-cli/Cargo.toml crates/ra-cli/Cargo.toml
COPY crates/ra-pipeline/Cargo.toml crates/ra-pipeline/Cargo.toml
COPY crates/ra-plugin/Cargo.toml crates/ra-plugin/Cargo.toml
COPY crates/app-skills/news/Cargo.toml crates/app-skills/news/Cargo.toml
COPY crates/app-skills/deep-search/Cargo.toml crates/app-skills/deep-search/Cargo.toml
COPY crates/app-skills/deep-crawl/Cargo.toml crates/app-skills/deep-crawl/Cargo.toml
COPY crates/app-skills/send-email/Cargo.toml crates/app-skills/send-email/Cargo.toml
COPY crates/app-skills/account-manager/Cargo.toml crates/app-skills/account-manager/Cargo.toml
COPY crates/app-skills/time/Cargo.toml crates/app-skills/time/Cargo.toml
COPY crates/app-skills/weather/Cargo.toml crates/app-skills/weather/Cargo.toml
COPY crates/platform-skills/voice/Cargo.toml crates/platform-skills/voice/Cargo.toml

# Create stub source files for dependency caching
# Library crates get lib.rs, binary crates get main.rs
RUN mkdir -p crates/ra-core/src && echo "" > crates/ra-core/src/lib.rs && \
    mkdir -p crates/ra-llm/src && echo "" > crates/ra-llm/src/lib.rs && \
    mkdir -p crates/ra-memory/src && echo "" > crates/ra-memory/src/lib.rs && \
    mkdir -p crates/ra-agent/src && echo "" > crates/ra-agent/src/lib.rs && \
    mkdir -p crates/ra-bus/src && echo "" > crates/ra-bus/src/lib.rs && \
    mkdir -p crates/ra-cli/src && echo "fn main() {}" > crates/ra-cli/src/main.rs && \
    mkdir -p crates/ra-pipeline/src && echo "" > crates/ra-pipeline/src/lib.rs && \
    mkdir -p crates/ra-plugin/src && echo "" > crates/ra-plugin/src/lib.rs && \
    mkdir -p crates/app-skills/news/src && echo "fn main() {}" > crates/app-skills/news/src/main.rs && \
    mkdir -p crates/app-skills/deep-search/src && echo "fn main() {}" > crates/app-skills/deep-search/src/main.rs && \
    mkdir -p crates/app-skills/deep-crawl/src && echo "fn main() {}" > crates/app-skills/deep-crawl/src/main.rs && \
    mkdir -p crates/app-skills/send-email/src && echo "fn main() {}" > crates/app-skills/send-email/src/main.rs && \
    mkdir -p crates/app-skills/account-manager/src && echo "fn main() {}" > crates/app-skills/account-manager/src/main.rs && \
    mkdir -p crates/app-skills/time/src && echo "fn main() {}" > crates/app-skills/time/src/main.rs && \
    mkdir -p crates/app-skills/weather/src && echo "fn main() {}" > crates/app-skills/weather/src/main.rs && \
    mkdir -p crates/platform-skills/voice/src && echo "fn main() {}" > crates/platform-skills/voice/src/main.rs

RUN cargo build --release --bin ra \
      -p ra-cli \
      --features api,telegram,discord,slack,whatsapp,feishu,email,audio_mp3 \
      2>/dev/null || true

# Copy full source and build
COPY . .
RUN find crates -name '*.rs' -exec touch {} + && \
    cargo build --release --bin ra \
      -p ra-cli \
      --features api,telegram,discord,slack,whatsapp,feishu,email,matrix,audio_mp3

# ============================================================
# Stage 2: Minimal runtime image
# ============================================================
FROM alpine:3.21

RUN apk add --no-cache ca-certificates tzdata \
    # Runtime deps for skills (pptx, mofa-pptx, browser)
    nodejs npm ffmpeg chromium \
    # LibreOffice + Poppler for office document conversion and visual QA
    libreoffice poppler-utils \
    # GCC for soffice sandbox shim (compiled on first use if needed)
    gcc musl-dev

# Install Node.js skill dependencies
RUN npm install -g pptxgenjs react-icons react react-dom sharp

# Copy binary
COPY --from=builder /src/target/release/ra /usr/local/bin/ra

# Copy builtin skills
COPY --from=builder /src/crates/ra-agent/skills /opt/ra/skills

# Create workspace
RUN mkdir -p /root/.ra/skills && \
    cp -r /opt/ra/skills/* /root/.ra/skills/ 2>/dev/null || true

ENTRYPOINT ["ra"]
CMD ["gateway"]
