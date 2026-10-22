#!/usr/bin/env bash
# Dedicated disposable Ubuntu24.04 ARM64 benchmark guest only. No host services.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
bench_root=${BENCH_ROOT:-/opt/ku-bench}
bench_user=${BENCH_USER:-mac}
install -d "$bench_root/tools" "$bench_root/build" "$bench_root/results"
chown -R "$bench_user":"$(id -gn "$bench_user")" "$bench_root"
apt-get update
apt-get install -y --no-install-recommends build-essential clang-18 libpq-dev postgresql-16 postgresql-client-16 pkg-config ca-certificates curl xz-utils git python3 python3-venv procps iproute2 linux-tools-common
apt-get install -y --no-install-recommends "linux-tools-$(uname -r)" || apt-get install -y --no-install-recommends linux-tools-generic
# The package-created cluster is not benchmark data and must not compete with it.
systemctl stop postgresql || true
cd "$bench_root/tools"
curl --fail --location --retry 2 --max-time 180 'https://nodejs.org/dist/v24.18.0/SHASUMS256.txt' -o node-SHASUMS256.txt
curl --fail --location --retry 2 --max-time 300 'https://nodejs.org/dist/v24.18.0/node-v24.18.0-linux-arm64.tar.xz' -o node.tar.xz
node_sum=$(awk '$2=="node-v24.18.0-linux-arm64.tar.xz" {print $1}' node-SHASUMS256.txt)
test ${#node_sum} -eq 64
printf '%s  node.tar.xz\n' "$node_sum" | sha256sum --check
tar -xf node.tar.xz
curl --fail --location --retry 2 --max-time 180 'https://go.dev/dl/?mode=json&include=all' -o go-releases.json
python3 - <<'PY'
import json
from pathlib import Path
version=next(x for x in json.loads(Path('go-releases.json').read_text()) if x['version']=='go1.27.2')
item=next(x for x in version['files'] if x['filename']=='go1.27.2.linux-arm64.tar.gz')
Path('go-pinned.json').write_text(json.dumps(item,indent=2)+'\n')
Path('go.sha256').write_text(item['sha256']+'  go.tar.gz\n')
PY
curl --fail --location --retry 2 --max-time 300 'https://dl.google.com/go/go1.27.2.linux-arm64.tar.gz' -o go.tar.gz
sha256sum --check go.sha256
tar -xf go.tar.gz
curl --fail --location --retry 2 --max-time 180 'https://static.rust-lang.org/rustup/dist/aarch64-unknown-linux-gnu/rustup-init' -o rustup-init
curl --fail --location --retry 2 --max-time 180 'https://static.rust-lang.org/rustup/dist/aarch64-unknown-linux-gnu/rustup-init.sha256' -o rustup-init.sha256
sha256sum --check rustup-init.sha256
chmod +x rustup-init
chown -R "$bench_user":"$(id -gn "$bench_user")" "$bench_root/tools"
runuser -u "$bench_user" -- env RUSTUP_HOME="$bench_root/tools/rustup" CARGO_HOME="$bench_root/tools/cargo" ./rustup-init --no-modify-path -y --profile minimal --default-toolchain 1.99.0
runuser -u "$bench_user" -- env RUSTUP_HOME="$bench_root/tools/rustup" CARGO_HOME="$bench_root/tools/cargo" "$bench_root/tools/cargo/bin/rustup" toolchain install 1.89.0 --profile minimal
{
 uname -a
 lscpu
 cat /sys/fs/cgroup/cgroup.controllers
 cat /sys/devices/system/cpu/online
 clang-18 --version
 "$bench_root/tools/go/bin/go" version
 "$bench_root/tools/node-v24.18.0-linux-arm64/bin/node" --version
 dpkg-query -W -f='${Package}=${Version}\n'
} > "$bench_root/tools/environment.txt"
# Retain verified tool downloads as dependency cache, never in repository source.
printf 'Dedicated cross-stack toolchains provisioned at %s\n' "$bench_root"
