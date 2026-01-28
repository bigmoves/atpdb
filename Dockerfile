# Multi-stage build for optimized Rust image
FROM rust:1.93.0-slim as builder

# Install system dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /app

# Copy Cargo files
COPY Cargo.toml Cargo.lock ./

# Create src directory with empty main.rs to cache dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs

# Cache dependencies (this layer only rebuilds when Cargo.toml changes)
RUN cargo build --release

# Copy actual source code
COPY src ./src

# Build the application
RUN cargo build --release

# Production stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create app user
RUN useradd -r -s /bin/false atpdb

# Create data directory
RUN mkdir -p /app/data && chown atpdb:atpdb /app/data

# Set working directory
WORKDIR /app

# Copy binary from builder stage
COPY --from=builder /app/target/release/atpdb ./atpdb

# Set permissions
RUN chown atpdb:atpdb /app/atpdb

# Switch to non-root user
USER atpdb

# Expose port for HTTP server
EXPOSE 3000

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD ./atpdb --help > /dev/null 2>&1 || exit 1

# Default command (can be overridden)
CMD ["./atpdb", "serve", "--port", "3000", "--connect"]