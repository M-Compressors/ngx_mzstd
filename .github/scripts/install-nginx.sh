# Installs the nginx that a Publish target is built for. Sourced by
# package-build.sh and package-test.sh inside the target's container.
#
# GROUP     nginx-org: nginx $NGINX_VERSION from nginx.org's $CHANNEL repository for
#           $OS/$RELEASE (debian, ubuntu, rhel or amzn);
#           distro: the distribution's own nginx, which must be $NGINX_VERSION.
# EXTRA_PACKAGES, if set, names packages to install along with it (`.` takes
# no arguments in every shell).
#
# Sets PKG_FORMAT (deb or rpm) and NGINX_EVR, the installed nginx package's
# version ([epoch:]version-release).

set -eu

EXTRA_PACKAGES=${EXTRA_PACKAGES:-}

if command -v apt-get >/dev/null; then
    PKG_FORMAT=deb
    export DEBIAN_FRONTEND=noninteractive
    apt_get() { apt-get -y -qq -o=Dpkg::Use-Pty=0 --no-install-recommends "$@"; }
else
    PKG_FORMAT=rpm
    dnf_() { dnf -y -q --setopt=install_weak_deps=False "$@"; }
fi

# nginx.org's signing keys, as https://nginx.org/en/linux_packages.html lists
# them.
NGINX_ORG_KEYS="573BFD6B3D8FBC641079A6ABABF5BD827BD9BF62
8540A6F18833A80E9C1653A42FD21310B49F6B46
9E9BE90EACBCDE69FE9B204CBCDCD8A38D88A2B3"

# Downloads nginx.org's signing key to $1 and checks that it holds only the
# keys above.
fetch_nginx_org_key() {
    curl -sSfL https://nginx.org/keys/nginx_signing.key -o "$1"
    fingerprints=$(gpg --show-keys --with-colons "$1" | sed -n 's/^fpr:*\([0-9A-F]*\):$/\1/p')
    [ -n "$fingerprints" ] || {
        echo "no keys in nginx_signing.key" >&2
        exit 1
    }
    for fpr in $fingerprints; do
        echo "$NGINX_ORG_KEYS" | grep -qx "$fpr" || {
            echo "unexpected key $fpr in nginx_signing.key" >&2
            exit 1
        }
    done
}

nginx_org_url() {
    if [ "$CHANNEL" = mainline ]; then
        echo "https://nginx.org/packages/mainline/$OS"
    else
        echo "https://nginx.org/packages/$OS"
    fi
}

install_nginx_org_deb() {
    apt-get update -qq
    apt_get install ca-certificates curl gpg "$@" >/dev/null
    fetch_nginx_org_key /tmp/nginx_signing.key
    keyring=/usr/share/keyrings/nginx-archive-keyring.gpg
    gpg --dearmor </tmp/nginx_signing.key >"$keyring"
    echo "deb [signed-by=$keyring] $(nginx_org_url) $RELEASE nginx" \
        >/etc/apt/sources.list.d/nginx.list
    apt-get update -qq
    # The distribution has an nginx too; name nginx.org's version.
    deb=$(apt-cache madison nginx |
        awk -v v="$NGINX_VERSION" '$3 ~ "^" v "-" && /nginx\.org/ { print $3; exit }')
    [ -n "$deb" ] || {
        echo "nginx.org has no nginx $NGINX_VERSION for $OS/$RELEASE" >&2
        exit 1
    }
    apt_get install "nginx=$deb" >/dev/null
}

install_nginx_org_rpm() {
    # Amazon Linux has curl-minimal, which conflicts with curl.
    command -v curl >/dev/null || dnf_ install curl >/dev/null
    command -v gpg >/dev/null || dnf_ install gnupg2 >/dev/null
    key=/etc/pki/rpm-gpg/RPM-GPG-KEY-nginx
    fetch_nginx_org_key "$key"
    cat >/etc/yum.repos.d/nginx.repo <<EOF
[nginx-$CHANNEL]
name=nginx $CHANNEL repo
baseurl=$(nginx_org_url)/$RELEASE/\$basearch/
gpgcheck=1
enabled=1
gpgkey=file://$key
module_hotfixes=true
EOF
    # The distribution's nginx has a higher epoch; name nginx.org's version.
    dnf_ install "nginx-$NGINX_VERSION" "$@" >/dev/null
}

install_distro() {
    if [ "$PKG_FORMAT" = deb ]; then
        apt-get update -qq
        apt_get install nginx "$@" >/dev/null
    else
        dnf_ install nginx "$@" >/dev/null
    fi
}

# EXTRA_PACKAGES is a list of package names, split into words on purpose.
# shellcheck disable=SC2086
case $GROUP-$PKG_FORMAT in
nginx-org-deb) install_nginx_org_deb $EXTRA_PACKAGES ;;
nginx-org-rpm) install_nginx_org_rpm $EXTRA_PACKAGES ;;
distro-*) install_distro $EXTRA_PACKAGES ;;
*)
    echo "unknown GROUP: $GROUP" >&2
    exit 1
    ;;
esac

installed=$(nginx -v 2>&1 | sed -n 's|^nginx version: nginx/\([0-9.]*\).*|\1|p')
if [ "$installed" != "$NGINX_VERSION" ]; then
    echo "installed nginx $installed, the target is nginx $NGINX_VERSION" >&2
    exit 1
fi

if [ "$PKG_FORMAT" = deb ]; then
    NGINX_EVR=$(dpkg-query -W -f='${Version}' nginx)
else
    NGINX_EVR=$(rpm -q --qf '%{EPOCHNUM}:%{VERSION}-%{RELEASE}' nginx)
fi
echo "installed nginx $NGINX_EVR"
