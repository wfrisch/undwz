# Build a self-contained, Ghidra-friendly ELF for Debian's /usr/sbin/sshd.
#
# Runs gather_for_ghidra.py, which locates sshd's detached debug info (from
# openssh-server-dbgsym), normalizes the DWARF with undwz, and merges it back
# into a single ELF at /work/usr.sbin.sshd.ghidra.
FROM debian:forky-slim

# Build + runtime dependencies:
#   rustc, cargo      - build undwz
#   build-essential   - `cc` linker for cargo (see .cargo/config.toml)
#   binutils          - readelf, objcopy (used by gather_for_ghidra.py + undwz)
#   elfutils          - eu-unstrip
#   python3           - run gather_for_ghidra.py
#   curl, patch, tar  - setup-vendor.sh materializes vendor/gimli
#   ca-certificates   - HTTPS for the crate download in setup-vendor.sh
RUN apt-get update && apt-get install -y --no-install-recommends \
        rustc cargo build-essential binutils elfutils python3 \
        curl patch tar ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Enable Debian's debug-symbol archive, then install sshd and its detached
# debug info. VERSION_CODENAME comes from the base image's /etc/os-release so
# this tracks whatever "stable" currently resolves to.
RUN . /etc/os-release && \
    echo "deb http://deb.debian.org/debian-debug ${VERSION_CODENAME}-debug main" \
        > /etc/apt/sources.list.d/debug.list && \
    apt-get update && \
    apt-get install -y --no-install-recommends \
        openssh-server openssh-server-dbgsym \
    && rm -rf /var/lib/apt/lists/*

# Build undwz. The repo (build context) is copied in; drop any host-built
# target/vendor so the container builds them fresh and correct.
WORKDIR /opt/undwz
COPY . .
RUN rm -rf target vendor && ./setup-vendor.sh && cargo build --release

# Emit the result into the shared /work volume.
WORKDIR /work
CMD ["python3", "/opt/undwz/gather_for_ghidra.py", \
     "/usr/sbin/sshd", "-o", "/work/usr.sbin.sshd.ghidra"]
