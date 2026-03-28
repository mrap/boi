FROM ubuntu:24.04

# Avoid interactive prompts
ENV DEBIAN_FRONTEND=noninteractive

# Install system dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    git \
    python3 \
    python3-pip \
    ca-certificates \
    nodejs \
    npm \
    && rm -rf /var/lib/apt/lists/*

# Install Claude CLI
RUN npm install -g @anthropic-ai/claude-code

# Create non-root worker user
RUN useradd -m -s /bin/bash worker
USER worker
WORKDIR /home/worker

# Entrypoint handles auth setup and launches the run script
COPY --chown=worker:worker entrypoint.sh /home/worker/entrypoint.sh
RUN chmod +x /home/worker/entrypoint.sh

ENTRYPOINT ["/home/worker/entrypoint.sh"]
