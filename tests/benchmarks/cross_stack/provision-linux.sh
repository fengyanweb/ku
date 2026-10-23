#!/usr/bin/env bash
# Dedicated disposable Ubuntu24.04 ARM64 benchmark guest only. No host services.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
bench_root=${BENCH_ROOT:-/opt/ku-bench}
bench_user=${BENCH_USER:-mac}
install -d "$bench_root/tools" "$bench_root/build" "$bench_root/results"
chown -R "$bench_user":"$(id -gn "$bench_user")" "$bench_root"
apt-get update
# Explicit tool/library versions selected from the maintained Ubuntu24.04
# repository on2026-10-23. Missing archived packages fail rather than silently
# changing a frozen experiment to a different JDK, compiler or PG minor.
apt-get install -y --no-install-recommends build-essential 'clang-18=1:18.1.3-1ubuntu1' 'llvm-18=1:18.1.3-1ubuntu1' 'lld-18=1:18.1.3-1ubuntu1' 'libclang-rt-18-dev=1:18.1.3-1ubuntu1' 'libpq-dev=16.15-0ubuntu0.24.04.1' 'postgresql-16=16.15-0ubuntu0.24.04.1' 'postgresql-client-16=16.15-0ubuntu0.24.04.1'  pkg-config ca-certificates curl xz-utils git python3 python3-venv procps iproute2 linux-tools-common 'cmake=3.28.3-1build7' 'libjsoncpp-dev=1.9.5-6build1' libssl-dev uuid-dev zlib1g-dev libbrotli-dev libc-ares-dev 'maven=3.8.7-2' 'openjdk-25-jdk-headless=25.0.4.1+1-1~24.04.4' 'openjdk-25-jre-headless=25.0.4.1+1-1~24.04.4'
apt-get install -y --no-install-recommends "linux-tools-$(uname -r)" || apt-get install -y --no-install-recommends linux-tools-generic
# The package-created cluster is not benchmark data and must not compete with it.
systemctl stop postgresql || true
cd "$bench_root/tools"
curl --fail --location --retry 2 --max-time 180 'https://nodejs.org/dist/v24.18.0/SHASUMS256.txt' -o node-SHASUMS256.txt
node_sum=$(awk '$2=="node-v24.18.0-linux-arm64.tar.xz" {print $1}' node-SHASUMS256.txt)
test ${#node_sum} -eq 64
if ! printf '%s  node.tar.xz\n' "$node_sum" | sha256sum --check --status 2>/dev/null; then
 curl --fail --location --retry 2 --max-time 300 'https://nodejs.org/dist/v24.18.0/node-v24.18.0-linux-arm64.tar.xz' -o node.tar.xz
fi
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
if ! sha256sum --check --status go.sha256 2>/dev/null; then
 curl --fail --location --retry 2 --max-time 300 'https://dl.google.com/go/go1.27.2.linux-arm64.tar.gz' -o go.tar.gz
fi
sha256sum --check go.sha256
tar -xf go.tar.gz
if ! sha256sum --check --status rustup-init.sha256 2>/dev/null; then
 curl --fail --location --retry 2 --max-time 180 'https://static.rust-lang.org/rustup/dist/aarch64-unknown-linux-gnu/rustup-init' -o rustup-init
 curl --fail --location --retry 2 --max-time 180 'https://static.rust-lang.org/rustup/dist/aarch64-unknown-linux-gnu/rustup-init.sha256' -o rustup-init.sha256
fi
sha256sum --check rustup-init.sha256
chmod +x rustup-init
chown -R "$bench_user":"$(id -gn "$bench_user")" "$bench_root/tools"
runuser -u "$bench_user" -- env RUSTUP_HOME="$bench_root/tools/rustup" CARGO_HOME="$bench_root/tools/cargo" ./rustup-init --no-modify-path -y --profile minimal --default-toolchain 1.89.0
runuser -u "$bench_user" -- env RUSTUP_HOME="$bench_root/tools/rustup" CARGO_HOME="$bench_root/tools/cargo" "$bench_root/tools/cargo/bin/rustup" component add llvm-tools-preview --toolchain 1.89.0

# Supported stable .NET10 LTS SDK; SHA512 from the official release metadata.
dotnet_hash=db85ae85527251fbb1290720ac3bf324bb1d9eea74516ef291ed4bb1f8ff6ec63a17593367f49b456115e8b24ead6d0c2ece2801e501f36a6cd4444ed01a498f
if ! printf '%s  dotnet-sdk.tar.gz\n' "$dotnet_hash" | sha512sum --check --status 2>/dev/null; then
 curl --fail --location --retry 2 --max-time 300 'https://builds.dotnet.microsoft.com/dotnet/Sdk/10.0.402/dotnet-sdk-10.0.402-linux-arm64.tar.gz' -o dotnet-sdk.tar.gz
fi
printf '%s  dotnet-sdk.tar.gz\n' "$dotnet_hash" | sha512sum --check
mkdir -p dotnet
tar -xf dotnet-sdk.tar.gz -C dotnet
# Build Drogon and the exact submodule source, never follow branch heads.
python3 - "$bench_root" <<'PYCPP'
import hashlib,json,pathlib,subprocess,sys,tarfile
root=pathlib.Path(sys.argv[1]);tools=root/'tools'
lock=json.loads((root/'src/tests/benchmarks/cross_stack/cpp/dependencies.lock.json').read_text())
source=tools/'drogon-src';source.mkdir(exist_ok=True)
for name,target in [('drogon',source),('trantor',source/'trantor')]:
 item=lock['sources'][name]; archive=tools/(name+'.tar.gz')
 if not archive.exists() or hashlib.sha256(archive.read_bytes()).hexdigest()!=item['archive_sha256']:
  subprocess.run(['curl','-fL','--retry','2','--max-time','180',item['archive_url'],'-o',str(archive)],check=True)
 if hashlib.sha256(archive.read_bytes()).hexdigest()!=item['archive_sha256']:raise RuntimeError('dependency hash mismatch: '+name)
 target.mkdir(exist_ok=True)
 subprocess.run(['tar','-xf',str(archive),'-C',str(target),'--strip-components=1'],check=True)
subprocess.run(['cmake','-S',str(source),'-B',str(tools/'drogon-cmake'),'-DCMAKE_BUILD_TYPE=Release','-DCMAKE_CXX_COMPILER=clang++-18','-DCMAKE_C_COMPILER=clang-18','-DCMAKE_CXX_FLAGS_RELEASE=-O3 -DNDEBUG','-DCMAKE_C_FLAGS_RELEASE=-O3 -DNDEBUG','-DCMAKE_INSTALL_PREFIX='+str(tools/'drogon'),'-DBUILD_EXAMPLES=OFF','-DBUILD_CTL=OFF','-DBUILD_TESTING=OFF','-DBUILD_ORM=ON','-DBUILD_BROTLI=OFF'],check=True)
subprocess.run(['cmake','--build',str(tools/'drogon-cmake'),'-j2'],check=True)
subprocess.run(['cmake','--install',str(tools/'drogon-cmake')],check=True)
versions=subprocess.check_output(['dpkg-query','-W','-f=${Package}=${Version}\n','libjsoncpp-dev','libpq-dev','openjdk-25-jdk-headless','cmake','clang-18'],text=True)
receipt={'sources':lock['sources'],'system_dependencies':versions,'flags':'Release -O3 -DNDEBUG; clang++18; full async PostgreSQL support'}
(tools/'drogon-build.json').write_text(json.dumps(receipt,indent=2)+'\n')
PYCPP
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
