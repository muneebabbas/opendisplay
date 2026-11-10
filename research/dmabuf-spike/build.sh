#!/bin/sh
# Generates Wayland protocol code and builds the spike.
set -e
cd "$(dirname "$0")"
P=/usr/share/wayland-protocols
mkdir -p gen
for x in staging/ext-image-capture-source/ext-image-capture-source-v1 \
         staging/ext-image-copy-capture/ext-image-copy-capture-v1 \
         staging/ext-foreign-toplevel-list/ext-foreign-toplevel-list-v1 \
         stable/linux-dmabuf/linux-dmabuf-v1; do
  n=$(basename $x)
  wayland-scanner client-header $P/$x.xml gen/$n-client.h
  wayland-scanner private-code  $P/$x.xml gen/$n.c
done
cc -O1 -g -Wall -Wno-unused-parameter -Igen -o spike spike.c gen/*.c \
  $(pkg-config --cflags --libs wayland-client gbm libva libva-drm libdrm)
