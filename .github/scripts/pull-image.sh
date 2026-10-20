#!/bin/sh
# Pulls one of Docker's official images, such as debian:trixie, and tags it
# under that plain name, trying Docker Hub and two of its mirrors in turn:
# each registry limits anonymous pulls by address, and the addresses of
# GitHub's runners are shared.
#
# usage: pull-image.sh <image>

set -eu

image=$1

if docker image inspect "$image" >/dev/null 2>&1; then
    exit 0
fi

for attempt in 1 2 3 4; do
    for registry in docker.io/library mirror.gcr.io/library public.ecr.aws/docker/library; do
        if docker pull -q "$registry/$image" >/dev/null 2>&1; then
            [ "$registry/$image" = "$image" ] || docker tag "$registry/$image" "$image"
            echo "pulled $image from $registry"
            exit 0
        fi
        echo "cannot pull $image from $registry" >&2
    done
    sleep $((attempt * 15))
done

echo "cannot pull $image from any registry" >&2
exit 1
