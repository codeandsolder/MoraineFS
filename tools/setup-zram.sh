#!/usr/bin/env bash
set -euo pipefail

MOUNT=/mnt/morainefs-hot
STATE=/run/morainefs/zram.state
LOGICAL_BYTES=1073741824
MEM_LIMIT_BYTES=209715200
ALGO=lz4hc

find_free_zram() {
  local z size
  for z in /sys/block/zram*; do
    [[ -e "$z/disksize" ]] || continue
    size="$(cat "$z/disksize")"
    if [[ "$size" -eq 0 ]]; then
      printf "/dev/%s\n" "$(basename "$z")"
      return 0
    fi
  done
  return 1
}

start() {
  local mode dev name id
  mkdir -p "$MOUNT"
  modprobe zram

  if mountpoint -q "$MOUNT"; then
    dev="$(findmnt -rn -o SOURCE "$MOUNT")"
    printf "adopted %s\n" "$dev" >"$STATE"
    exit 0
  fi

  # Re-adopt only our explicitly recorded device. Never guess from arbitrary
  # ext4-formatted zram devices; test/other-service zrams may coexist.
  if [[ -f "$STATE" ]]; then
    read -r mode dev <"$STATE" || true
    if [[ -n "${dev:-}" && -b "$dev" ]] &&
       [[ "$(cat "/sys/block/$(basename "$dev")/disksize" 2>/dev/null || echo 0)" -gt 0 ]] &&
       [[ "$(blkid -p -s TYPE -o value "$dev" 2>/dev/null || true)" == ext4 ]] &&
       ! findmnt -rn -S "$dev" >/dev/null 2>&1; then
      mount -o relatime,nodiratime,discard "$dev" "$MOUNT"
      mount --make-private "$MOUNT"
      printf "%s %s\n" "${mode:-adopted}" "$dev" >"$STATE"
      exit 0
    fi
    rm -f "$STATE"
  fi

  if dev="$(find_free_zram)"; then
    :
  else
    id="$(cat /sys/class/zram-control/hot_add)"
    dev="/dev/zram${id}"
  fi

  name="$(basename "$dev")"
  echo "$ALGO" >"/sys/block/$name/comp_algorithm"
  echo "$LOGICAL_BYTES" >"/sys/block/$name/disksize"
  echo "$MEM_LIMIT_BYTES" >"/sys/block/$name/mem_limit"
  mkfs.ext4 -q -F -m 0 -O ^has_journal "$dev"
  mount -o relatime,nodiratime,discard "$dev" "$MOUNT"
  mount --make-private "$MOUNT"
  printf "owned %s\n" "$dev" >"$STATE"
}

stop() {
  local mode dev name
  [[ -f "$STATE" ]] || exit 0
  read -r mode dev <"$STATE" || true

  if mountpoint -q "$MOUNT"; then
    umount "$MOUNT"
  fi

  if [[ "$mode" == owned && -n "${dev:-}" ]]; then
    name="$(basename "$dev")"
    if [[ -e "/sys/block/$name/reset" ]]; then
      echo 1 >"/sys/block/$name/reset" || true
    fi
  fi
  rm -f "$STATE"
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  *) echo "usage: $0 {start|stop}" >&2; exit 2 ;;
esac
