#!/bin/sh
# Builds the package of one Publish target inside the target's container:
# installs its nginx, configures that nginx version's sources with the
# arguments of `nginx -V`, builds the module against them and packs it with
# nfpm (packaging/nfpm.yaml).
#
# usage: package-build.sh <out-dir>
#
# The repository is at /src (read-only is fine). The target comes from the
# environment, as publish-matrix.py describes it: GROUP, CHANNEL, OS, RELEASE,
# CODENAME, NGINX_VERSION, ARCH.
#
# nginx-org targets build the upstream tarball, which is what nginx.org
# packages; distro targets build the distribution's source package, with its
# patches, since a patch can change the layout of a structure the module uses.

set -eu

out=$1
src=/src
work=/tmp/ngx_mzstd-build
scripts=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$out" "$work"

# nfpm, checked against the release's checksums.txt.
NFPM_VERSION=2.47.0
case $(uname -m) in
x86_64)
    nfpm_archive=nfpm_${NFPM_VERSION}_Linux_x86_64.tar.gz
    nfpm_sha256=0660ca602b2d2d2ae4781a06c692b3eeb9d437ffea05b831d76e41f4a3188783
    ;;
aarch64)
    nfpm_archive=nfpm_${NFPM_VERSION}_Linux_arm64.tar.gz
    nfpm_sha256=1c0f5f2999b9a974bfb04fdb0cc3306096de530ac5dbb25d739cc5f5219c919c
    ;;
*)
    echo "no nfpm for $(uname -m)" >&2
    exit 1
    ;;
esac

if command -v apt-get >/dev/null; then
    deps="gcc make libc6-dev libclang-dev libpcre2-dev zlib1g-dev libssl-dev"
    [ "$GROUP" = distro ] && deps="$deps dpkg-dev"
else
    deps="gcc make pcre2-devel zlib-devel openssl-devel clang-devel tar gzip findutils"
    if [ "$GROUP" = distro ]; then
        deps="$deps rpm-build gnupg2 dnf-command(download)"
        # %prep checks the tarball's signature with %gpgverify (gnupg2) or,
        # since Fedora 43, with %openpgpverify, which this package defines.
        if [ -n "$(dnf -q repoquery openpgpverify 2>/dev/null)" ]; then
            deps="$deps openpgpverify"
        fi
    fi
fi
export EXTRA_PACKAGES="$deps"
# shellcheck source-path=SCRIPTDIR
. "$scripts/install-nginx.sh"
command -v curl >/dev/null || {
    if [ "$PKG_FORMAT" = deb ]; then apt_get install curl ca-certificates >/dev/null; else dnf_ install curl >/dev/null; fi
}

# The arguments nginx was configured with. `nginx -V` quotes them as a shell
# would, so let the shell split them.
args=$(nginx -V 2>&1 | sed -n 's/^configure arguments: //p')
eval "set -- $args"
configure=
have_compat=false
modules_path=
for arg; do
    case $arg in
    # Compiler and linker options matter to the binary, not to the module, and
    # name tools (such as Red Hat's -specs files) the container may lack.
    --with-cc-opt=* | --with-ld-opt=*) continue ;;
    # Third-party modules bundled with the distribution's package, and
    # dynamic modules that would need their libraries' headers. With
    # --with-compat they do not change the structures the module uses; the
    # signature check below catches a module that defines a macro that does.
    --add-module=* | --add-dynamic-module=* | *=dynamic) continue ;;
    --with-compat) have_compat=true ;;
    --modules-path=*) modules_path=${arg#--modules-path=} ;;
    --with-google_perftools_module)
        if [ "$PKG_FORMAT" = rpm ]; then dnf_ install gperftools-devel >/dev/null; else apt_get install libgoogle-perftools-dev >/dev/null; fi
        ;;
    esac
    configure="$configure '$(printf '%s' "$arg" | sed "s/'/'\\\\''/g")'"
done
if [ "$have_compat" = false ]; then
    configure="$configure --with-compat"
fi
[ -n "$modules_path" ] || {
    echo "nginx -V names no --modules-path" >&2
    exit 1
}

# The sources of the installed nginx.
cd "$work"
if [ "$GROUP" = nginx-org ]; then
    curl -sSfL "https://nginx.org/download/nginx-$NGINX_VERSION.tar.gz" | tar xz
    nginx_src=$work/nginx-$NGINX_VERSION
elif [ "$PKG_FORMAT" = deb ]; then
    # Enable the deb-src entries next to the deb ones.
    for list in /etc/apt/sources.list.d/*.sources; do
        [ -e "$list" ] && sed -i 's/^Types: deb$/Types: deb deb-src/' "$list"
    done
    if [ -e /etc/apt/sources.list ]; then
        sed -n 's/^deb /deb-src /p' /etc/apt/sources.list >/etc/apt/sources.list.d/deb-src.list
    fi
    apt-get update -qq
    apt-get source -qq --only-source "nginx=$(dpkg-query -W -f='${source:Version}' nginx)" >/dev/null
    nginx_src=$work/nginx-$NGINX_VERSION
else
    dnf -q download --source "nginx-$(rpm -q --qf '%{VERSION}-%{RELEASE}' nginx)"
    rpm -i --nosignature --define "_topdir $work/rpmbuild" ./nginx-*.src.rpm 2>/dev/null
    # %prep checks the tarball's signature and applies the patches.
    rpmbuild -bp --nodeps --define "_topdir $work/rpmbuild" "$work"/rpmbuild/SPECS/nginx.spec \
        >"$work/prep.log" 2>&1 || {
        tail -50 "$work/prep.log" >&2
        exit 1
    }
    nginx_src=$(find "$work/rpmbuild/BUILD" -type d -name "nginx-$NGINX_VERSION" | head -1)
fi
[ -x "$nginx_src/configure" ] || {
    echo "no nginx sources in $nginx_src" >&2
    exit 1
}

echo "configure$configure"
(cd "$nginx_src" && eval "./configure $configure" >"$work/configure.log" 2>&1) || {
    tail -50 "$work/configure.log" >&2
    exit 1
}

# Rust from rustup: distributions ship older releases than rust-version.
# Cargo refuses a toolchain older than rust-version itself.
if ! command -v cargo >/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y -q --profile minimal --default-toolchain "${RUST_TOOLCHAIN:-stable}"
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi
rustc --version

export CARGO_TARGET_DIR=$work/target
(cd "$src" && NGINX_SOURCE_DIR=$nginx_src NGINX_BUILD_DIR=$nginx_src/objs cargo build --release --locked)
so=$CARGO_TARGET_DIR/release/libngx_mzstd.so

# NGINX refuses a module whose signature differs from its own; check here
# rather than in the package's users.
signature() { grep -ao '[0-9],[0-9],[0-9],[01]\{30,\}' "$1" | head -1; }
want=$(signature "$(command -v nginx)")
have=$(signature "$so")
if [ "$want" != "$have" ]; then
    echo "module signature $have differs from nginx's $want" >&2
    exit 1
fi
echo "signature $have"

# Package metadata.
module=$(sed -n 's/^version = "\(.*\)"/\1/p' "$src/Cargo.toml" | head -1)
PKG_DESCRIPTION=$(sed -n 's/^description = "\(.*\)"/\1/p' "$src/Cargo.toml" | head -1)
PKG_MAINTAINER=$(sed -n 's/^authors = \["\([^"]*\)".*/\1/p' "$src/Cargo.toml" | head -1)
PKG_ARCH=$ARCH
NGINX_MODULES_PATH=$modules_path
PKG_CONFLICTS=
if [ "$PKG_FORMAT" = rpm ]; then
    dist=$(rpm --eval '%{?dist}')
fi

stage=$work/stage
rm -rf "$stage"
mkdir -p "$stage/scripts" "$stage/tree"
cp "$scripts/../../packaging/scripts/$GROUP/$PKG_FORMAT/"* "$stage/scripts/"

case $GROUP-$PKG_FORMAT in
nginx-org-deb)
    PKG_NAME=nginx-module-mzstd
    PKG_VERSION=$NGINX_VERSION+$module
    PKG_RELEASE=1~$CODENAME
    PKG_DEPENDS="nginx (= $NGINX_EVR)"
    PKG_CONFLICTS=nginx-module-zstd
    ;;
nginx-org-rpm)
    PKG_NAME=nginx-module-mzstd
    PKG_VERSION=$NGINX_VERSION+$module
    PKG_RELEASE=1$dist.ngx
    PKG_DEPENDS="nginx = $NGINX_EVR"
    PKG_CONFLICTS=nginx-module-zstd
    ;;
distro-deb)
    PKG_NAME=libnginx-mod-http-mzstd
    PKG_VERSION=$module+$NGINX_VERSION
    PKG_RELEASE=1~$CODENAME
    # The ABI that the distribution's modules depend on, such as
    # nginx-abi-1.26.3-1.
    PKG_DEPENDS=$(dpkg-query -W -f='${Provides}' nginx | tr ',' '\n' | sed -n 's/^ *\(nginx-abi-[^ ]*\).*/\1/p')
    [ -n "$PKG_DEPENDS" ] || {
        echo "nginx provides no nginx-abi-*" >&2
        exit 1
    }
    if apt-cache show libnginx-mod-http-zstd >/dev/null 2>&1; then
        PKG_CONFLICTS=libnginx-mod-http-zstd
    fi
    mkdir -p "$stage/tree/usr/share/nginx/modules-available"
    echo 'load_module modules/ngx_mzstd.so;' \
        >"$stage/tree/usr/share/nginx/modules-available/mod-http-mzstd.conf"
    ;;
distro-rpm)
    PKG_NAME=nginx-mod-mzstd
    PKG_VERSION=$module+$NGINX_VERSION
    PKG_RELEASE=1$dist
    # Such as "nginx(abi) = 1.26.3", which Fedora's nginx-core provides
    # rather than nginx.
    abi_provider=$(rpm -q --whatprovides 'nginx(abi)' | head -1)
    PKG_DEPENDS=$(rpm -q --provides "$abi_provider" | grep -m1 '^nginx(abi) = ' || true)
    [ -n "$PKG_DEPENDS" ] || {
        echo "nginx provides no nginx(abi)" >&2
        exit 1
    }
    if [ -n "$(dnf -q repoquery nginx-mod-zstd 2>/dev/null)" ]; then
        PKG_CONFLICTS=nginx-mod-zstd
    fi
    mkdir -p "$stage/tree/usr/share/nginx/modules"
    echo "load_module \"$modules_path/ngx_mzstd.so\";" \
        >"$stage/tree/usr/share/nginx/modules/mod-mzstd.conf"
    ;;
esac
export PKG_NAME PKG_VERSION PKG_RELEASE PKG_ARCH PKG_DEPENDS PKG_CONFLICTS \
    PKG_DESCRIPTION PKG_MAINTAINER NGINX_MODULES_PATH

cp "$so" "$stage/ngx_mzstd.so"
cp "$src/LICENSE" "$src/CHANGELOG.md" "$stage/"

curl -sSfL "https://github.com/goreleaser/nfpm/releases/download/v$NFPM_VERSION/$nfpm_archive" \
    -o "$work/$nfpm_archive"
echo "$nfpm_sha256  $work/$nfpm_archive" | sha256sum -c -
tar -xzf "$work/$nfpm_archive" -C "$work" nfpm

cd "$stage"
"$work/nfpm" package --config "$src/packaging/nfpm.yaml" --packager "$PKG_FORMAT" --target "$out/"
ls -l "$out"
