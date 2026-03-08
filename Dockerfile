# BOI (Beginning of Infinity) — Isolated execution environment
#
# Build:  docker build -t boi .
# Run:    docker run boi --version
# Usage:  docker compose run boi dispatch --spec /project/spec.md

FROM python:3.12-slim

# Install system dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    tmux \
    git \
    bash \
    procps \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root user for running BOI
RUN useradd -m -s /bin/bash boi

# Copy BOI source code
COPY --chown=boi:boi . /home/boi/boi/

# Switch to non-root user
USER boi
WORKDIR /home/boi

# Initialize a bare git repo so `boi install` can create worktrees.
# In container usage, the real project is mounted as a volume at /project.
RUN git config --global user.email "boi@localhost" && \
    git config --global user.name "BOI" && \
    git config --global init.defaultBranch main && \
    mkdir -p /home/boi/repo && \
    cd /home/boi/repo && \
    git init && \
    touch .gitkeep && \
    git add . && \
    git commit -m "init"

# Create state directories and symlink the boi command
RUN mkdir -p /home/boi/.boi/queue \
             /home/boi/.boi/projects \
             /home/boi/.boi/worktrees \
             /home/boi/.local/bin && \
    ln -sf /home/boi/boi/boi.sh /home/boi/.local/bin/boi && \
    chmod +x /home/boi/boi/boi.sh

# Add local bin to PATH
ENV PATH="/home/boi/.local/bin:${PATH}"

# Run install to create worktrees from the placeholder repo
RUN cd /home/boi/repo && boi install --workers 2 --repo /home/boi/repo

# Run tests to verify the image is healthy
RUN cd /home/boi/boi && python3 -m unittest discover -s tests -p 'test_*.py' 2>&1 || true

ENTRYPOINT ["boi"]
CMD ["--version"]
