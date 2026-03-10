# Stage 1: release-build - Release compilation
FROM rust:1.93.1 AS release-build

WORKDIR /usr/src/app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

# Build the web server binary in release mode
RUN cargo clean
RUN cargo build --release --bin aura-web-server

# Stage 2: release - Runtime stage with newer glibc
FROM debian:trixie-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create app user for security
RUN useradd -r -s /bin/false appuser

# Create app directory, config directory, and skills directory
WORKDIR /app
RUN mkdir -p /app/config /app/skills && chown -R appuser:appuser /app

# Copy binary from release-build stage
COPY --from=release-build /usr/src/app/target/release/aura-web-server /app/

# Switch to app user
USER appuser

# Expose port
EXPOSE 3030

# Set default environment variables
ENV HOST=0.0.0.0
ENV PORT=3030
ENV CONFIG_PATH=/app/config/config.toml
ENV AURA_SKILLS_DIR=/app/skills

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:3030/health || exit 1

# Run the web server
CMD ["./aura-web-server"]
